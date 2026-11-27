import type { ProfileCode, ProfileInstruction, ProfileSourceFile } from "../protocol";

export interface CodeRow {
  instruction: ProfileInstruction;

  offset: number;

  leads: boolean;

  jump: number | null;

  symbol: string | null;
}

function mnemonicOf(instruction: ProfileInstruction): string {
  return instruction.tokens.find(([kind]) => kind === "mnemonic")?.[1] ?? "";
}

function jumpTarget(instruction: ProfileInstruction): number | null {
  const last = instruction.tokens.at(-1);
  if (!mnemonicOf(instruction).startsWith("j") || last?.[0] !== "address") {
    return null;
  }
  const match = /^0x([0-9a-f]+)$/u.exec(last[1]);
  return match?.[1] === undefined ? null : Number.parseInt(match[1], 16);
}

const ENDS_BLOCK = /^(?:j[a-z]*|ret[a-z]*|ud2)$/u;

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

export interface TreeCall {
  key: string;

  function: number;

  at: TreeLine | null;

  lines: TreeLine[];
}

export interface TreeLine {
  key: string;
  call: TreeCall;

  file: number | null;

  line: number;

  own: number;

  inside: number;

  total: number;

  rows: number[];

  inlined: TreeCall[];
}

export interface InlineTree {
  calls: TreeCall[];

  paths: TreeLine[][];

  lines: Map<string, TreeLine>;
}

/**
 * Builds the inline call tree from disassembled instruction rows.
 * `instruction.lines` arrives innermost-to-outermost, so we walk it reversed;
 * `inside` accumulates inclusive weight across all inlined descendants while
 * `own` is credited only to the innermost line.
 */
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

export function isLibrary(path: string | null | undefined): boolean {
  return !path || LIBRARY.some((pattern) => pattern.test(path));
}

export function ownCodeAt(path: readonly TreeLine[], files: readonly ProfileSourceFile[]): number {
  for (let at = path.length - 1; at > 0; at--) {
    const file = path[at]?.file;
    if (!isLibrary(file === null || file === undefined ? null : files[file]?.path)) {
      return at;
    }
  }
  return 0;
}

export function inlinedFrom(
  tree: InlineTree,
  files: readonly ProfileSourceFile[],
): (number | null)[] {
  return tree.paths.map((path) => {
    const at = ownCodeAt(path, files) || (path.length > 1 ? 1 : 0);
    return at > 0 ? (path[at]?.call.function ?? null) : null;
  });
}

export type SourceEntry =
  | { kind: "line"; depth: number; line: TreeLine }
  | { kind: "text"; depth: number; file: number; number: number }
  | { kind: "gap"; depth: number; count: number }
  | { kind: "call"; depth: number; call: TreeCall };

export const FOLD = 4;

/**
 * Flattens the inline tree and source files into display entries for the source pane.
 * Top-level frames (`depth === 0`) include surrounding context lines; expanded
 * inline frames (`depth > 0`) fold gaps of at least `FOLD` untouched lines.
 */
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

export const TINTS = 6;

const HUES = [30, 140, 280, 350, 180, 55] as const;

const SHADES = [0, 28, -28, 56, -56, 84] as const;

function hottest(lines: readonly TreeLine[]): TreeLine[] {
  return lines
    .filter((line) => line.line > 0 && (line.inside > 0 || line.total > 0))
    .toSorted((a, b) => b.inside - a.inside || b.total - a.total);
}

/**
 * Assigns distinct hues to the top `TINTS` hottest lines at each level, shifting
 * expanded children by `SHADES` so source lines and their assembly rows stay
 * visually paired.
 */
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

export function linesAbove(line: TreeLine): string[] {
  const above: string[] = [];
  for (let at = line.call.at; at; at = at.call.at) {
    above.unshift(at.key);
  }
  return above;
}

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

/**
 * Picks the initial line to focus when opening the source/assembly split.
 * Prefers the explicitly requested function (e.g. when viewing an inlined-only
 * callee); otherwise attributes each instruction to its innermost non-library
 * frame (`ownCodeAt`) so the view opens on user code rather than stdlib internals.
 */
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

export function shortName(name: string): string {
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

function lastPart(path: string): string {
  let bare = path;
  for (let was = ""; was !== bare;) {
    was = bare;
    bare = bare.replaceAll(/<[^<>]*>/gu, "");
  }
  const parts = bare.split("::").filter((part) => part !== "");
  return parts.at(-1) ?? path;
}

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
