// How a function's instructions map to the lines of one source file: each
// instruction belongs to its innermost line in that file, the hottest
// lines get tints their instructions share, and a line's instructions fall
// into runs, since the compiler moves and repeats them.

import type {
  ProfileCode,
  ProfileCodeLine,
  ProfileInstruction,
  ProfileSourceFile,
} from "../protocol";

/** How many lines get a tint; past a handful, hues stop being told apart. */
export const TINTS = 6;

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

/** The instruction's line in the file at index `file` of the answer's
 * files: the innermost one there, or 0 when it has none there. Lines run
 * innermost inlined function first. */
export function lineIn(lines: readonly ProfileCodeLine[], file: number): number {
  for (const [, at, line] of lines) {
    if (at === file && line > 0) {
      return line;
    }
  }
  return 0;
}

/** The address a direct jump's text names, as `jne 0x401136` does. */
function jumpTarget(text: string): number | null {
  if (!/^j[a-z]*\s/u.test(text)) {
    return null;
  }
  const match = /0x([0-9a-f]+)$/u.exec(text);
  return match?.[1] === undefined ? null : Number.parseInt(match[1], 16);
}

const ENDS_BLOCK = /^(?:j[a-z]*|ret[a-z]*|ud2)(?:\s|$)/u;

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
    const target = jumpTarget(instruction.text);
    if (target !== null) {
      targets.add(target);
    }
  }
  return instructions.map(({ instruction, start, symbol }, index) => {
    const target = jumpTarget(instruction.text);
    const before = instructions[index - 1]?.instruction;
    return {
      instruction,
      offset: instruction.address - start,
      leads:
        index > 0 &&
        (instruction.address === start ||
          targets.has(instruction.address) ||
          (before !== undefined && ENDS_BLOCK.test(before.text))),
      jump: target !== null && held.has(target) ? target : null,
      symbol,
    };
  });
}

/** A file's lines something ran on, hottest first: by own weight, then
 * total, then the earlier line. */
function hottest(file: ProfileSourceFile): number[] {
  return file.lines
    .filter(([, own, total]) => own > 0 || total > 0)
    .toSorted((a, b) => b[1] - a[1] || b[2] - a[2] || a[0] - b[0])
    .map(([line]) => line);
}

/** The tints of a file's hottest lines, by line: 0 for the hottest. */
export function tintsOf(file: ProfileSourceFile, count = TINTS): Map<number, number> {
  return new Map(
    hottest(file)
      .slice(0, count)
      .map((line, tint) => [line, tint]),
  );
}

/** The runs of consecutive rows whose line is `line`, first and last
 * index, in listing order. */
export function runsOf(lines: readonly number[], line: number): [number, number][] {
  const runs: [number, number][] = [];
  lines.forEach((at, index) => {
    if (at !== line) {
      return;
    }
    const last = runs.at(-1);
    if (last && last[1] === index - 1) {
      last[1] = index;
    } else {
      runs.push([index, index]);
    }
  });
  return runs;
}

/** The line to open on: the file's hottest of `own`, the lines of the
 * function asked for, or else the file's hottest line, or else the first
 * line any instruction has. */
export function openingLine(
  file: ProfileSourceFile | undefined,
  lines: readonly number[],
  own: ReadonlySet<number> = new Set(),
): number {
  const ranked = file ? hottest(file) : [];
  return ranked.find((line) => own.has(line)) ?? ranked[0] ?? lines.find((line) => line > 0) ?? 0;
}

/** The lines in the file at index `file` that the function at index
 * `function_` of the answer's functions has in any row. */
export function linesOf(rows: readonly CodeRow[], function_: number, file: number): Set<number> {
  const own = new Set<number>();
  for (const row of rows) {
    for (const [at, inFile, line] of row.instruction.lines) {
      if (at === function_ && inFile === file && line > 0) {
        own.add(line);
      }
    }
  }
  return own;
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

/** A function's name without its path or generic arguments, as
 * `<core::result::Result<&[u8; 8], E>>::copied` reads `copied`, for where
 * a chain of them must fit on one line. */
export function shortName(name: string): string {
  let bare = name;
  for (let was = ""; was !== bare;) {
    was = bare;
    bare = bare.replaceAll(/<[^<>]*>/gu, "");
  }
  const parts = bare.split("::").filter((part) => part !== "");
  return parts.at(-1) ?? name;
}
