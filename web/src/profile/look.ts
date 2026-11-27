import { assert } from "../assert";
import type {
  ProfileDuring,
  ProfileGranularity,
  ProfileQuery,
  ProfileTransform,
  ProfileZoneScope,
  SyscallGroup,
  SyscallQuery,
  TransformOp,
} from "../protocol";
import type { DiffMode } from "./paint";

export type ProfileView = "flame" | "tree" | "functions" | "kernel" | "zones" | "details";

export type SyscallTab = "log" | "files" | "network" | "processes";

/**
 * Compact URL query-parameter schema for profile views.
 *
 * Short keys keep shareable links small:
 * - scope/filters (shared): `range`, `th` (threads), `tk` (tasks), `src` (waits), `lb` (labels), `bl` (root by label), `tf` (transforms), `fn` (selected function)
 * - zones (shared): `zs` (site), `zb` (duration band), `zf` (scope: all/band), `zi` (single instance), `bz` (diff band vs rest)
 * - diff (shared): `bp` (base profile), `br` (base range), `dm` (time vs share)
 * - syscalls: `sf` (filter), `sl` (latency bucket), `du` (during), `sk` (syscall leaves) are shared; `st` (tab), `sg` (group), `sc` (selected call), `ev` (event) are view-local
 * - display (view-local, except `view`): `view`, `z` (zoom path), `invert`, `by`, `stacks`, `bt`, `ann`, `lanes`
 */
export interface ProfileLook {
  view?: ProfileView;

  range?: string;

  th?: string[];

  src?: "waits";

  tk?: string[];

  bt?: "1";

  lb?: string[];

  bl?: string;

  zs?: string;

  zb?: string;

  zf?: "all" | "band";

  zi?: string;

  bz?: "rest";

  lanes?: "tasks";

  tf?: string[];
  invert?: "1";

  stacks?: "tasks" | "native";

  by?: Exclude<ProfileGranularity, "function">;

  fn?: string;

  z?: string[];

  bp?: string;

  br?: string;

  dm?: "share";

  ann?: "lines" | "code";

  sf?: string;

  sg?: Exclude<SyscallGroup, "syscall">;

  sc?: string;

  st?: SyscallTab;

  ev?: string;

  sl?: string;

  du?: string;

  sk?: "1";
}

const VIEWS: readonly ProfileView[] = ["flame", "tree", "functions", "kernel", "zones", "details"];
const SYSCALL_GROUPS: ReadonlySet<string> = new Set(["class", "process", "thread", "path", "site"]);
const SYSCALL_TABS: ReadonlySet<string> = new Set(["log", "files", "network", "processes"]);
const GRANULARITIES: ReadonlySet<string> = new Set(["physical", "line", "address"]);
const OPS: readonly TransformOp[] = ["focus", "merge", "drop"];

const NAMELESS: readonly TransformOp[] = ["collapseCycles"];

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
  if (view && (VIEWS as readonly string[]).includes(view)) {
    look.view = view as ProfileView;
  }
  const range = text("range");
  if (range && parseRange(range)) {
    look.range = range;
  }
  const th = list("th")?.filter((row) => /^\d+$/u.test(row));
  if (th && th.length > 0) {
    look.th = th;
  }
  if (text("src") === "waits") {
    look.src = "waits";
  }
  const tk = list("tk")?.filter((row) => row === "none" || /^\d+$/u.test(row));
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
  if (look.zs) {
    const zb = text("zb");
    if (zb && parseBand(zb)) {
      look.zb = zb;
    }
    const zf = text("zf");
    if (zf === "all" || (zf === "band" && look.zb)) {
      look.zf = zf;
    }
    if (text("bz") === "rest" && look.zb) {
      look.bz = "rest";
    }
  }
  const zi = text("zi");
  if (zi && parseInstance(zi)) {
    look.zi = zi;
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
  const stacks = text("stacks");
  if (stacks === "tasks" || stacks === "native") {
    look.stacks = stacks;
  }
  const by = text("by");
  if (by && GRANULARITIES.has(by)) {
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
  if (text("dm") === "share") {
    look.dm = "share";
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
  if (sg && SYSCALL_GROUPS.has(sg)) {
    look.sg = sg as ProfileLook["sg"] & string;
  }
  const st = text("st");
  if (st && SYSCALL_TABS.has(st)) {
    look.st = st as SyscallTab;
  }
  const ev = text("ev");
  if (ev && /^\d+$/u.test(ev)) {
    look.ev = ev;
  }
  const sl = text("sl");
  if (sl && /^\d{1,2}$/u.test(sl)) {
    look.sl = sl;
  }
  const du = text("du");
  if (du && (du === "all" || parseDuringRow(du))) {
    look.du = du;
  }
  if (text("sk") === "1") {
    look.sk = "1";
  }
  return look;
}

export function parseDuringRow(text: string): { group: SyscallGroup; key: string } | null {
  const at = text.indexOf(":");
  const group = text.slice(0, at);
  return at > 0 && at < text.length - 1 && (group === "syscall" || SYSCALL_GROUPS.has(group))
    ? { group: group as SyscallGroup, key: text.slice(at + 1) }
    : null;
}

export function formatDuringRow(group: SyscallGroup, key: string): string {
  return `${group}:${key}`;
}

export function duringLabel(du: string): string {
  const row = parseDuringRow(du);
  if (!row) {
    return "calls";
  }
  return row.group === "process" || row.group === "thread" ? `${row.group} ${row.key}` : row.key;
}

export function duringOf(look: ProfileLook): ProfileDuring | null {
  if (!look.du) {
    return null;
  }
  const row = look.du === "all" ? null : parseDuringRow(look.du);
  return {
    filter: look.sf ?? "",
    ...(look.sl ? { latency: Number(look.sl) } : {}),
    ...(row ? { row } : {}),
  };
}

export function syscallQueryOf(profile: string, look: ProfileLook, revision: number): SyscallQuery {
  const { base: _base, during: _during, ...link } = queryOf(profile, look);
  return {
    ...link,
    syscallLeaves: false,
    revision,
    ...(look.sl ? { latency: Number(look.sl) } : {}),
    filter: look.sf ?? "",
  };
}

export function parseRange(text: string): [number, number] | null {
  const match = /^(\d{1,16})-(\d{1,16})$/u.exec(text);
  if (!match) {
    return null;
  }
  const start = Number(match[1]);
  const end = Number(match[2]);
  return end > start ? [start, end] : null;
}

export function parseBand(text: string): [number, number] | null {
  const match = /^(\d{1,16})-(\d{1,16})$/u.exec(text);
  if (!match) {
    return null;
  }
  const low = Number(match[1]);
  const high = Number(match[2]);
  return low <= high ? [low, high] : null;
}

export function formatBand([low, high]: readonly [number, number]): string {
  assert(low <= high, `band ${low}..${high} is reversed`);
  return `${Math.round(low)}-${Math.round(high)}`;
}

export interface Instance {
  thread: number;
  span: [number, number];
}

export function parseInstance(text: string): Instance | null {
  const at = text.indexOf(":");
  const span = parseRange(text.slice(at + 1));
  return at > 0 && /^\d{1,10}$/u.test(text.slice(0, at)) && span
    ? { thread: Number(text.slice(0, at)), span }
    : null;
}

export function formatInstance({ thread, span }: Instance): string {
  return `${thread}:${formatRange(span)}`;
}

export function withInstance(look: ProfileLook, instance: Instance | null): ProfileLook {
  return withLook(look, "zi", instance ? formatInstance(instance) : undefined);
}

/**
 * Steps back one level of zone selection on Escape: single instance (`zi`) ->
 * duration band (`zb`) -> zone site (`zs`).
 */
export function cleared(look: ProfileLook): ProfileLook {
  if (look.zi) {
    return withInstance(look, null);
  }
  const { zb, zf, bz: _bz, zs: _zs, ...rest } = look;
  if (zb) {
    return { ...rest, ...(look.zs ? { zs: look.zs } : {}), ...(zf === "all" ? { zf } : {}) };
  }
  return look.zs ? rest : look;
}

export function withoutSite(look: ProfileLook): ProfileLook {
  const { zs: _zs, zb: _zb, zf: _zf, bz: _bz, zi: _zi, ...rest } = look;
  return rest;
}

export function scopeOf(look: ProfileLook): ProfileZoneScope | null {
  if (!look.zs || look.zi || look.src === "waits") {
    return null;
  }
  const band = look.zb ? parseBand(look.zb) : null;
  if (band && (look.zf === "band" || look.bz === "rest")) {
    return { name: look.zs, band, outside: false };
  }
  return look.zf === "all" ? { name: look.zs, band: null, outside: false } : null;
}

export function diffModeOf(look: { dm?: "share" | undefined }): DiffMode {
  return look.dm === "share" ? "share" : "time";
}

export function rangeOf(look: { range?: string | undefined }): [number, number] | null {
  return look.range ? parseRange(look.range) : null;
}

export function spanOf(look: ProfileLook): [number, number] | null {
  return (look.zi ? parseInstance(look.zi)?.span : null) ?? rangeOf(look);
}

export function formatRange([start, end]: [number, number]): string {
  assert(start <= end, `range ${start}..${end} is reversed`);
  return `${Math.round(start)}-${Math.round(end)}`;
}

export function parseTransform(entry: string): ProfileTransform | null {
  if ((NAMELESS as readonly string[]).includes(entry)) {
    return { op: entry as TransformOp, name: "" };
  }
  const at = entry.indexOf(":");
  const op = entry.slice(0, at);
  const name = entry.slice(at + 1);
  return at > 0 && name !== "" && (OPS as readonly string[]).includes(op)
    ? { op: op as TransformOp, name }
    : null;
}

function formatTransform(transform: ProfileTransform): string {
  return NAMELESS.includes(transform.op) ? transform.op : `${transform.op}:${transform.name}`;
}

export function queryOf(profile: string, look: ProfileLook): ProfileQuery {
  const range = spanOf(look);
  const zones = scopeOf(look);
  const baseRange = look.br ? parseRange(look.br) : null;
  const during = duringOf(look);

  // Comparing a zone band against the rest of the same site (`outside: true`)
  // takes precedence over profile/range diff baselines (`bp`/`br`).
  const rest = look.bz === "rest" && zones?.band ? { ...zones, outside: true } : null;
  const base = rest
    ? { profile, ...(range ? { range } : {}), zones: rest }
    : look.bp || baseRange
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
    look: look.stacks ?? "code",
    granularity: look.by ?? "function",
    ...(zones ? { zones } : {}),
    ...(base ? { base } : {}),
    ...(during ? { during } : {}),
    syscallLeaves: look.sk === "1",
  };
}

export function transformed(look: ProfileLook, transform: ProfileTransform): ProfileLook {
  const { z: _z, ...rest } = look;
  return { ...rest, tf: [...(look.tf ?? []), formatTransform(transform)] };
}

export function untransformed(look: ProfileLook, index: number): ProfileLook {
  const { tf, z: _z, ...rest } = look;
  assert(
    index >= 0 && index < (tf?.length ?? 0),
    `transform ${index} is outside ${tf?.length ?? 0} transforms`,
  );
  const kept = (tf ?? []).toSpliced(index, 1);
  return kept.length > 0 ? { ...rest, tf: kept } : rest;
}

const OWN = [
  "z",
  "invert",
  "by",
  "stacks",
  "bt",
  "ann",
  "lanes",
  "st",
  "sg",
  "sc",
  "ev",
] as const satisfies readonly (keyof ProfileLook)[];

export type OwnLook = Pick<ProfileLook, (typeof OWN)[number]>;

/**
 * Splits a `ProfileLook` into shared query scope (stored on the `ProfileLink`)
 * and view-local display state (`OWN`, stored per pane) so linked panes share
 * filters/range without clobbering each other's zoom, inversion, or granularity.
 */
export function splitLook(look: ProfileLook): { shared: ProfileLook; own: OwnLook } {
  const { view: _view, ...rest } = look;
  const shared: Record<string, unknown> = { ...rest };
  const own: Record<string, unknown> = {};
  for (const key of OWN) {
    if (key in shared) {
      own[key] = shared[key];
      delete shared[key];
    }
  }
  return { shared: shared as ProfileLook, own: own as OwnLook };
}

export function withLook<K extends keyof ProfileLook>(
  look: ProfileLook,
  key: K,
  value: ProfileLook[K] | undefined,
): ProfileLook {
  const { [key]: _old, ...rest } = look;
  return value === undefined ? (rest as ProfileLook) : ({ ...rest, [key]: value } as ProfileLook);
}
