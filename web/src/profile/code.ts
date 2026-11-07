// How a function's instructions map to the source they were compiled
// from: a tree of its lines, and under each the functions the compiler
// inlined there, deeper. The hottest lines get tints their instructions
// share, each run of instructions inlined from another function is
// bracketed with its name, and a line's instructions fall into runs, since
// the compiler moves and repeats them.

import type { ProfileCode, ProfileInstruction, ProfileSourceFile } from "../protocol";

/** An instruction of the listing, in address order across its blocks. */
export interface CodeRow {
  instruction: ProfileInstruction;

  /** Its address less its block's first, the start of its symbol. */
  offset: number;

  /** Whether a basic block starts here: after a jump or return, or where
   * one goes. */
  leads: boolean;

  /** The address a direct jump goes to, when the listing holds it. */
  jump: number | null;

  /** Its symbol, when it is the first instruction of its block. */
  symbol: string | null;
}

/** The instruction's mnemonic, without its prefixes. */
function mnemonicOf(instruction: ProfileInstruction): string {
  return instruction.tokens.find(([kind]) => kind === "mnemonic")?.[1] ?? "";
}

/** The address a direct jump goes to: its operand, when that is an
 * address, as in `jne 0x401136`. */
function jumpTarget(instruction: ProfileInstruction): number | null {
  const last = instruction.tokens.at(-1);
  if (!mnemonicOf(instruction).startsWith("j") || last?.[0] !== "address") {
    return null;
  }
  const match = /^0x([0-9a-f]+)$/u.exec(last[1]);
  return match?.[1] === undefined ? null : Number.parseInt(match[1], 16);
}

const ENDS_BLOCK = /^(?:j[a-z]*|ret[a-z]*|ud2)$/u;

/** The listing's rows: every block's instructions in order, with where
 * basic blocks begin. */
export function codeRows(code: ProfileCode): CodeRow[] {
  const instructions = code.blocks.flatMap((block) =>
    block.instructions.map((instruction, at) => ({
      instruction,
      start: block.instructions[0]?.address ?? instruction.address,
      symbol: at === 0 ? block.symbol : null,
    })),
  );
  const held = new Set(instructions.map(({ instruction }) => instruction.address));
  const targets = new Set<number>();
  for (const { instruction } of instructions) {
    const target = jumpTarget(instruction);
    if (target !== null) {
      targets.add(target);
    }
  }
  return instructions.map(({ instruction, start, symbol }, index) => {
    const target = jumpTarget(instruction);
    const before = instructions[index - 1]?.instruction;
    return {
      instruction,
      offset: instruction.address - start,
      leads:
        index > 0 &&
        (instruction.address === start ||
          targets.has(instruction.address) ||
          (before !== undefined && ENDS_BLOCK.test(mnemonicOf(before)))),
      jump: target !== null && held.has(target) ? target : null,
      symbol,
    };
  });
}

/** A function in the inline tree: one of the listing's own, or one the
 * compiler inlined at a line of another. */
export interface TreeCall {
  /** The calls and lines that lead to it, unique in the tree. */
  key: string;

  /** Its index in the answer's functions. */
  function: number;

  /** The line it was inlined at; null for the listing's own functions. */
  at: TreeLine | null;

  /** Its lines that hold instructions, in file and line order, with
   * those of no line last. */
  lines: TreeLine[];
}

/** A line of a function in the inline tree. The same line is a
 * different one wherever its function was inlined. */
export interface TreeLine {
  /** The calls and lines that lead to it, unique in the tree. */
  key: string;
  call: TreeCall;

  /** Its file's index in the answer's files, or null when unknown. */
  file: number | null;

  /** 0 when the debug information names none. */
  line: number;

  /** The own weight of the instructions it is the innermost line of. */
  own: number;

  /** The own weight of every instruction under it, inlined or not. */
  inside: number;

  /** The total weight of every instruction under it. */
  total: number;

  /** The listing's rows under it, in order. */
  rows: number[];

  /** The functions inlined at it, heaviest first. */
  inlined: TreeCall[];
}

/** The listing's instructions as a tree: each of the listing's functions
 * holds its lines, and each line the functions inlined at it. */
export interface InlineTree {
  /** The listing's own functions, in the order their code comes. */
  calls: TreeCall[];

  /** Each row's lines, outermost first; empty when it has none. */
  paths: TreeLine[][];

  /** Every line, by key. */
  lines: Map<string, TreeLine>;
}

/** The inline tree of the listing's rows. */
export function inlineTree(rows: readonly CodeRow[]): InlineTree {
  const calls = new Map<string, TreeCall>();
  const lines = new Map<string, TreeLine>();
  const top: TreeCall[] = [];
  const callOf = (function_: number, at: TreeLine | null): TreeCall => {
    const key = `${at ? `${at.key}>` : ""}${function_}`;
    let call = calls.get(key);
    if (!call) {
      call = { key, function: function_, at, lines: [] };
      calls.set(key, call);
      (at ? at.inlined : top).push(call);
    }
    return call;
  };

  const paths = rows.map(({ instruction }, row) => {
    const path: TreeLine[] = [];
    let at: TreeLine | null = null;
    // The lines run innermost first; the tree, outermost first.
    for (const [function_, file, number] of instruction.lines.toReversed()) {
      const call = callOf(function_, at);
      const key = `${call.key}@${file ?? ""}:${number}`;
      let line = lines.get(key);
      if (!line) {
        line = {
          key,
          call,
          file,
          line: number,
          own: 0,
          inside: 0,
          total: 0,
          rows: [],
          inlined: [],
        };
        lines.set(key, line);
        call.lines.push(line);
      }
      line.inside += instruction.own;
      line.total += instruction.total;
      line.rows.push(row);
      path.push(line);
      at = line;
    }
    const innermost = path.at(-1);
    if (innermost) {
      innermost.own += instruction.own;
    }
    return path;
  });

  const inside = (call: TreeCall) => call.lines.reduce((sum, line) => sum + line.inside, 0);
  const total = (call: TreeCall) => call.lines.reduce((sum, line) => sum + line.total, 0);
  for (const line of lines.values()) {
    line.inlined.sort((a, b) => inside(b) - inside(a) || total(b) - total(a));
  }
  for (const call of calls.values()) {
    call.lines.sort(
      (a, b) =>
        Number(a.line === 0) - Number(b.line === 0) ||
        (a.file ?? Infinity) - (b.file ?? Infinity) ||
        a.line - b.line,
    );
  }
  return { calls: top, paths, lines };
}

/** Paths of source a toolchain or a dependency provides, which are not the
 * program's own code: Rust's library, crates, C and C++ headers, Go's
 * modules, and Zig's library. */
const LIBRARY = [
  /\/lib\/rustlib\/src\/rust\/library\//u,
  /^\/rustc\/[0-9a-f]+\//u,
  /\/\.cargo\/(?:registry|git)\//u,
  /^\/usr\/(?:local\/)?include\//u,
  /^\/nix\/store\/[^/]+\/include\//u,
  /\/include\/c\+\+\//u,
  /\/lib\/gcc\//u,
  /\/pkg\/mod\//u,
  /\/lib\/zig\/std\//u,
];

/** Whether source at `path` is a toolchain's or a dependency's rather
 * than the program's own; unknown files count as not its own. */
export function isLibrary(path: string | null | undefined): boolean {
  return !path || LIBRARY.some((pattern) => pattern.test(path));
}

/** Where in `path`, the lines of an instruction outermost first, its
 * innermost inlined line of the program's own code is: 0, its function's
 * own line, when every inlined line is a library's. */
export function ownCodeAt(path: readonly TreeLine[], files: readonly ProfileSourceFile[]): number {
  for (let at = path.length - 1; at > 0; at--) {
    const file = path[at]?.file;
    if (!isLibrary(file === null || file === undefined ? null : files[file]?.path)) {
      return at;
    }
  }
  return 0;
}

/** The function each row's code was inlined from, as its bracket names
 * it: the innermost of the program's own, or else the outermost inlined;
 * null for a function's own code. */
export function inlinedFrom(
  tree: InlineTree,
  files: readonly ProfileSourceFile[],
): (number | null)[] {
  return tree.paths.map((path) => {
    const at = ownCodeAt(path, files) || (path.length > 1 ? 1 : 0);
    return at > 0 ? (path[at]?.call.function ?? null) : null;
  });
}

/** A row of the source pane: a line with code, one without, a fold of
 * lines without, or the head of an inlined function. */
export type SourceEntry =
  | { kind: "line"; depth: number; line: TreeLine }
  | { kind: "text"; depth: number; file: number; number: number }
  | { kind: "gap"; depth: number; count: number }
  | { kind: "call"; depth: number; call: TreeCall };

/** How many lines without code an inlined function's lines fold into one
 * row; fewer are shown, as they read with the code around them. */
export const FOLD = 4;

/** The source pane's rows: each of the listing's functions' lines, and
 * under each line `open` names the functions inlined at it, deeper. A
 * function's lines read as its file has them, the listing's own with all
 * the text read around them; an inlined function's long stretches without
 * code fold away. */
export function sourceEntries(
  tree: InlineTree,
  files: readonly ProfileSourceFile[],
  open: ReadonlySet<string>,
): SourceEntry[] {
  const entries: SourceEntry[] = [];
  const text = (file: number | null, number: number) => {
    const shown = file === null ? undefined : files[file];
    return shown?.text?.[number - shown.first] !== undefined;
  };

  const lineOf = (line: TreeLine, depth: number) => {
    entries.push({ kind: "line", depth, line });
    if (open.has(line.key)) {
      for (const call of line.inlined) {
        entries.push({ kind: "call", depth: depth + 1, call });
        callOf(call, depth + 1);
      }
    }
  };

  const callOf = (call: TreeCall, depth: number) => {
    const lines = call.lines;
    for (let at = 0; at < lines.length;) {
      const first = lines[at] as TreeLine;
      // A stretch of one file's text: its lines with code, and the lines
      // between them.
      let end = at + 1;
      if (first.line > 0 && text(first.file, first.line)) {
        while (
          end < lines.length &&
          lines[end]?.file === first.file &&
          (lines[end]?.line ?? 0) > 0 &&
          text(first.file, lines[end]?.line ?? 0)
        ) {
          end++;
        }
      }
      // A function's own lines read with the file's text around them.
      const around =
        depth === 0 && text(first.file, first.line) ? files[first.file ?? -1] : undefined;
      if (around) {
        for (let number = around.first; number < first.line; number++) {
          entries.push({ kind: "text", depth, file: first.file as number, number });
        }
      }
      for (let index = at; index < end; index++) {
        const line = lines[index] as TreeLine;
        const next = lines[index + 1];
        lineOf(line, depth);
        if (index + 1 === end || !next || first.file === null) {
          continue;
        }
        const between = next.line - line.line - 1;
        if (depth > 0 && between >= FOLD) {
          entries.push({ kind: "gap", depth, count: between });
        } else {
          for (let number = line.line + 1; number < next.line; number++) {
            entries.push({ kind: "text", depth, file: first.file, number });
          }
        }
      }
      if (around) {
        const last = lines[end - 1] as TreeLine;
        for (let number = last.line + 1; text(first.file, number); number++) {
          entries.push({ kind: "text", depth, file: first.file as number, number });
        }
      }
      at = end;
    }
  };

  for (const call of tree.calls) {
    if (tree.calls.length > 1) {
      entries.push({ kind: "call", depth: 0, call });
    }
    callOf(call, 0);
  }
  return entries;
}

/** How many lines get a tint at each level; past a handful, hues stop
 * being told apart. */
export const TINTS = 6;

/** The hues of the hottest lines of the listing's functions. */
const HUES = [30, 140, 280, 350, 180, 55] as const;

/** How far the hottest lines inlined at a tinted line turn from its hue. */
const SHADES = [0, 28, -28, 56, -56, 84] as const;

/** Source lines something ran on, hottest first: by the own weight under
 * them, then total, then the order they come in. Code of no source line
 * has a texture of its own rather than a hue. */
function hottest(lines: readonly TreeLine[]): TreeLine[] {
  return lines
    .filter((line) => line.line > 0 && (line.inside > 0 || line.total > 0))
    .toSorted((a, b) => b.inside - a.inside || b.total - a.total);
}

/** The hue of each tinted line, by key: the hottest lines of the
 * listing's functions, and under each tinted line `open` names, the
 * hottest lines inlined at it, in shades of its hue. */
export function tintsOf(tree: InlineTree, open: ReadonlySet<string>): Map<string, number> {
  const tints = new Map<string, number>();
  const shade = (lines: readonly TreeLine[], hue: (at: number) => number) => {
    for (const [at, line] of hottest(lines).slice(0, TINTS).entries()) {
      tints.set(line.key, hue(at));
    }
    for (const line of lines) {
      const tint = tints.get(line.key);
      if (tint !== undefined && open.has(line.key)) {
        shade(
          line.inlined.flatMap((call) => call.lines),
          (at) => (tint + (SHADES[at] ?? 0) + 360) % 360,
        );
      }
    }
  };
  shade(
    tree.calls.flatMap((call) => call.lines),
    (at) => HUES[at] ?? 0,
  );
  return tints;
}

/** The line an instruction whose lines are `path` shows under: the
 * deepest one whose callers `open` names. */
export function shownLine(
  path: readonly TreeLine[],
  open: ReadonlySet<string>,
): TreeLine | undefined {
  let at = 0;
  while (at + 1 < path.length && open.has(path[at]?.key ?? "")) {
    at++;
  }
  return path[at];
}

/** The hue of an instruction whose lines are `path`: its shown line's, or
 * the nearest tinted line's around it; none when it shows under no source
 * line. */
export function tintOfPath(
  path: readonly TreeLine[],
  open: ReadonlySet<string>,
  tints: ReadonlyMap<string, number>,
): number | undefined {
  const shown = shownLine(path, open);
  if (!shown || shown.line === 0) {
    return undefined;
  }
  for (let at = path.indexOf(shown); at >= 0; at--) {
    const tint = tints.get(path[at]?.key ?? "");
    if (tint !== undefined) {
      return tint;
    }
  }
  return undefined;
}

/** The keys of the lines a line was inlined under, outermost first, which
 * must be open to show it. */
export function linesAbove(line: TreeLine): string[] {
  const above: string[] = [];
  for (let at = line.call.at; at; at = at.call.at) {
    above.unshift(at.key);
  }
  return above;
}

/** Every line of the tree, each before the functions inlined at it. */
function everyLine(tree: InlineTree): TreeLine[] {
  const all: TreeLine[] = [];
  const walk = (call: TreeCall) => {
    for (const line of call.lines) {
      all.push(line);
      line.inlined.forEach(walk);
    }
  };
  tree.calls.forEach(walk);
  return all;
}

/** The line to open on: the hottest by own, then total, the first on a
 * tie. Of the function at index `function_` of the answer's when it was
 * only ever inlined, by its own lines' weights; or else of the program's
 * own code, each instruction weighed on its innermost line there, the
 * line clicking it chooses. Undefined when no line holds code. */
export function openingLine(
  tree: InlineTree,
  rows: readonly CodeRow[],
  files: readonly ProfileSourceFile[],
  function_ = -1,
): TreeLine | undefined {
  const all = everyLine(tree);
  const weights = new Map<TreeLine, [number, number]>();
  const asked = all.filter((line) => line.call.function === function_);
  if (asked.length > 0) {
    for (const line of asked) {
      weights.set(line, [line.own, line.total]);
    }
  } else {
    for (const [index, path] of tree.paths.entries()) {
      const line = path[ownCodeAt(path, files)];
      const instruction = rows[index]?.instruction;
      if (line && instruction) {
        const [own, total] = weights.get(line) ?? [0, 0];
        weights.set(line, [own + instruction.own, total + instruction.total]);
      }
    }
  }
  let best: TreeLine | undefined;
  let most: [number, number] = [-1, -1];
  for (const line of all) {
    const [own, total] = weights.get(line) ?? [-1, -1];
    if (own > most[0] || (own === most[0] && total > most[1])) {
      best = line;
      most = [own, total];
    }
  }
  return best;
}

/** The runs of consecutive rows among `rows`, given in order: each its
 * first and last index. */
export function runsOf(rows: readonly number[]): [number, number][] {
  const runs: [number, number][] = [];
  for (const row of rows) {
    const last = runs.at(-1);
    if (last && last[1] === row - 1) {
      last[1] = row;
    } else {
      runs.push([row, row]);
    }
  }
  return runs;
}

/** The index of the run whose instructions weigh the most own, then
 * total, the earliest on a tie; 0 when there are none. */
export function hottestRun(rows: readonly CodeRow[], runs: readonly [number, number][]): number {
  let best = 0;
  let most: [number, number] = [-1, -1];
  runs.forEach(([first, last], at) => {
    let own = 0;
    let total = 0;
    for (const row of rows.slice(first, last + 1)) {
      own += row.instruction.own;
      total += row.instruction.total;
    }
    if (own > most[0] || (own === most[0] && total > most[1])) {
      best = at;
      most = [own, total];
    }
  });
  return best;
}

/** A function's name without its paths or generic arguments, keeping the
 * type a method is of, as `<core::result::Result<&[u8; 8], E>>::copied`
 * reads `Result::copied` and `<Range<usize> as SliceIndex<[u8]>>::index`
 * reads `Range::index`, for where a name must fit in a little room. */
export function shortName(name: string): string {
  // A method's name starts with its type in angle brackets: `<Type>::` or
  // `<Type as Trait>::`.
  if (name.startsWith("<")) {
    let depth = 0;
    for (let at = 0; at < name.length; at++) {
      depth += name[at] === "<" ? 1 : name[at] === ">" ? -1 : 0;
      if (depth === 0 && name.startsWith(">::", at)) {
        const type = name.slice(1, at);
        const own = type.slice(0, outside(type, " as ") ?? type.length);
        return `${lastPart(own)}::${lastPart(name.slice(at + 3))}`;
      }
    }
  }
  return lastPart(name);
}

/** The last `::` part of a path, without generic arguments. */
function lastPart(path: string): string {
  let bare = path;
  for (let was = ""; was !== bare;) {
    was = bare;
    bare = bare.replaceAll(/<[^<>]*>/gu, "");
  }
  const parts = bare.split("::").filter((part) => part !== "");
  return parts.at(-1) ?? path;
}

/** Where `part` first comes in `text` outside angle brackets. */
function outside(text: string, part: string): number | undefined {
  let depth = 0;
  for (let at = 0; at < text.length; at++) {
    if (depth === 0 && text.startsWith(part, at)) {
      return at;
    }
    depth += text[at] === "<" ? 1 : text[at] === ">" ? -1 : 0;
  }
  return undefined;
}
