// How a function's instructions map to the source they were compiled
// from: where its basic blocks begin, the tree of its lines and what was
// inlined at each, which lines get a tint, how runs of inlined code are
// named, and how a line's instructions fall into runs.

import * as fc from "fast-check";
import { describe, expect, it } from "vitest";

import {
  type CodeRow,
  codeRows,
  FOLD,
  hottestRun,
  inlinedFrom,
  inlineTree,
  isLibrary,
  linesAbove,
  openingLine,
  ownCodeAt,
  runsOf,
  shortName,
  shownLine,
  type SourceEntry,
  sourceEntries,
  TINTS,
  tintOfPath,
  tintsOf,
  type TreeLine,
} from "../src/profile/code";
import type { ProfileCode, ProfileInstruction, ProfileSourceFile } from "../src/protocol";

/** An instruction whose text, such as `jne 0x10`, is tokenized as the
 * decoder does: its mnemonic, then an address operand or other text. */
function instruction(address: number, text: string, own = 0, total = own): ProfileInstruction {
  const [mnemonic = "", operand] = text.split(" ");
  const tokens: [string, string][] = [["mnemonic", mnemonic]];
  if (operand !== undefined) {
    tokens.push(["text", " "], [/^0x[0-9a-f]+$/u.test(operand) ? "address" : "text", operand]);
  }
  return { address, tokens, target: null, own, total, lines: [] };
}

function code(...blocks: [string, ProfileInstruction[]][]): ProfileCode {
  return {
    weight: 100,
    module: "app",
    blocks: blocks.map(([symbol, instructions]) => ({ symbol, instructions, stopped: null })),
    unplaced: [],
    unavailable: null,
    functions: [],
    files: [],
    unlined: null,
    inlined_into: null,
  };
}

describe("codeRows", () => {
  const rows = codeRows(
    code(
      [
        "scan",
        [
          instruction(0x100, "push rbp"),
          instruction(0x101, "cmp rdi, 0"),
          instruction(0x105, "je 0x110"),
          instruction(0x107, "add rax, 1"),
          // Jumps out of the listing: no row to follow.
          instruction(0x10b, "jmp 0x9000"),
          instruction(0x110, "jne 0x101"),
          instruction(0x112, "ret"),
          // Reached only by a jump from elsewhere, if at all.
          instruction(0x113, "nop"),
        ],
      ],
      ["scan.cold", [instruction(0x2000, "ud2"), instruction(0x2002, "nop")]],
    ),
  );
  const at = (address: number): CodeRow => {
    const row = rows.find((each) => each.instruction.address === address);
    expect(row).toBeDefined();
    return row as CodeRow;
  };

  it("starts a basic block after each jump and return and at each jump's target", () => {
    expect(rows.filter((row) => row.leads).map((row) => row.instruction.address)).toEqual([
      // jne's target.
      0x101,
      // After je.
      0x107,
      // After jmp; also je's target.
      0x110,
      // After jne.
      0x112,
      // After ret.
      0x113,
      // The next symbol.
      0x2000,
      // After ud2.
      0x2002,
    ]);
  });

  it("measures offsets from each block's own start", () => {
    expect(at(0x112).offset).toBe(0x12);
    expect(at(0x2002).offset).toBe(2);
    expect(at(0x2000).symbol).toBe("scan.cold");
    expect(at(0x100).symbol).toBe("scan");
    expect(at(0x101).symbol).toBeNull();
  });

  it("follows a jump only to an address it shows", () => {
    expect(at(0x105).jump).toBe(0x110);
    expect(at(0x110).jump).toBe(0x101);
    expect(at(0x10b).jump).toBeNull();
    expect(at(0x107).jump).toBeNull();
  });
});

/** scan in app.c (file 0), whose line 4 runs step inlined, which runs
 * get from Rust's library (file 1) inlined at its line 14; line 8 runs
 * step inlined again. Lines run innermost first, as the server sends them. */
const FUNCTIONS = ["scan", "step", "<core::option::Option<u8>>::get"];
const FILES: ProfileSourceFile[] = [
  {
    path: "/src/app.c",
    lines: [],
    first: 1,
    text: Array.from({ length: 30 }, (_, index) => `line ${index + 1}`),
    unread: null,
  },
  {
    path: "/nix/store/abc-rust/lib/rustlib/src/rust/library/core/src/option.rs",
    lines: [],
    first: 1,
    text: null,
    unread: "unread",
  },
];
const ROWS = codeRows(
  code([
    "scan",
    [
      { ...instruction(0, "mov", 1, 1), lines: [[0, 0, 3]] },
      {
        ...instruction(1, "add", 5, 5),
        lines: [
          [2, 1, 9],
          [1, 0, 14],
          [0, 0, 4],
        ],
      },
      { ...instruction(2, "call", 0, 7), lines: [[0, 0, 4]] },
      {
        ...instruction(3, "imul", 2, 2),
        lines: [
          [1, 0, 19],
          [0, 0, 4],
        ],
      },
      {
        ...instruction(4, "sub", 3, 3),
        lines: [
          [1, 0, 14],
          [0, 0, 8],
        ],
      },
      { ...instruction(5, "ret"), lines: [[0, 0, 18]] },
      // Only library code inlined here.
      {
        ...instruction(6, "nop", 0, 0),
        lines: [
          [2, 1, 9],
          [0, 0, 18],
        ],
      },
    ],
  ]),
);
const TREE = inlineTree(ROWS);

/** The tree's line of `function` at `number`, reached through `path`,
 * the lines above it outermost first. */
function lineAt(...path: [number, number][]): TreeLine {
  let lines = TREE.calls.flatMap((call) => call.lines);
  let found: TreeLine | undefined;
  for (const [function_, number] of path) {
    found = lines.find((line) => line.call.function === function_ && line.line === number);
    expect(found, `${function_}:${number}`).toBeDefined();
    lines = found?.inlined.flatMap((call) => call.lines) ?? [];
  }
  return found as TreeLine;
}

describe("inlineTree", () => {
  it("nests each inlined function under the line it was inlined at", () => {
    expect(TREE.calls.map((call) => call.function)).toEqual([0]);
    expect(TREE.calls[0]?.lines.map((line) => line.line)).toEqual([3, 4, 8, 18]);

    const four = lineAt([0, 4]);
    // Its own instruction is the call, which spends nothing itself; the
    // rest is step's and get's.
    expect([four.own, four.inside, four.total]).toEqual([0, 7, 14]);
    expect(four.rows).toEqual([1, 2, 3]);
    expect(four.inlined.map((call) => call.function)).toEqual([1]);
    expect(lineAt([0, 4], [1, 14]).inside).toBe(5);
    expect(lineAt([0, 4], [1, 14]).own).toBe(0);
    expect(lineAt([0, 4], [1, 14], [2, 9]).own).toBe(5);
    expect(lineAt([0, 4], [1, 19]).own).toBe(2);
    expect(TREE.paths[1]?.map((line) => line.line)).toEqual([4, 14, 9]);
  });

  it("keeps a function inlined at two lines apart, line by line", () => {
    const here = lineAt([0, 4], [1, 14]);
    const there = lineAt([0, 8], [1, 14]);
    expect(here.key).not.toBe(there.key);
    expect(there.rows).toEqual([4]);
    expect(there.own).toBe(3);
    expect(here.rows).toEqual([1]);
  });

  /** Random chains: up to 4 deep, of 3 functions, 2 files, and 5 lines. */
  const chain = fc.array(
    fc.tuple(
      fc.integer({ min: 0, max: 2 }),
      fc.constantFrom(0, 1, null),
      fc.integer({ min: 0, max: 4 }),
    ),
    { minLength: 1, maxLength: 4 },
  );
  const listing = fc.array(fc.tuple(chain, fc.nat(9), fc.nat(9)), { maxLength: 30 }).map((each) =>
    codeRows(
      code([
        "f",
        each.map(([lines, own, more], index) => ({
          ...instruction(index, "nop", own, own + more),
          lines,
        })),
      ]),
    ),
  );

  it("weighs each instruction's own once, at its innermost line, and its total on every line above", () => {
    fc.assert(
      fc.property(listing, (rows) => {
        const tree = inlineTree(rows);
        const lines = [...tree.lines.values()];
        const sum = (of: (row: CodeRow) => number, among = rows) =>
          among.reduce((all, row) => all + of(row), 0);
        // One comparison per case: an expect for each line and step made the
        // 10,000 cases of `just web-props` run into the test timeout.
        const under = (line: (typeof lines)[number]) =>
          rows.filter((_, index) => tree.paths[index]?.includes(line));
        expect({
          own: lines.reduce((all, line) => all + line.own, 0),
          paths: tree.paths.map((path) => [
            path.length,
            // Each step down is a function inlined at the line above.
            path.every((line, depth) => line.call.at === (depth === 0 ? null : path[depth - 1])),
          ]),
          lines: lines.map((line) => [line.rows, line.inside, line.total, line.inside]),
        }).toEqual({
          own: sum((row) => row.instruction.own),
          paths: tree.paths.map((_, index) => [rows[index]?.instruction.lines.length, true]),
          lines: lines.map((line) => [
            under(line).map((row) => rows.indexOf(row)),
            sum((row) => row.instruction.own, under(line)),
            sum((row) => row.instruction.total, under(line)),
            // What runs under a line is its own and what was inlined there.
            line.own +
              line.inlined
                .flatMap((call) => call.lines)
                .reduce((all, each) => all + each.inside, 0),
          ]),
        });
      }),
    );
  });
});

describe("ownCodeAt and inlinedFrom", () => {
  it("knows a toolchain's and a dependency's source from the program's", () => {
    expect(isLibrary(FILES[1]?.path)).toBe(true);
    expect(isLibrary("/rustc/0a1b2c/library/core/src/slice/mod.rs")).toBe(true);
    expect(isLibrary("/home/me/.cargo/registry/src/index/serde-1.0/src/de.rs")).toBe(true);
    expect(isLibrary("/usr/include/stdlib.h")).toBe(true);
    expect(isLibrary("/usr/lib/gcc/x86_64-linux-gnu/14/include/c++/vector")).toBe(true);
    expect(isLibrary("/home/me/go/pkg/mod/golang.org/x/sys@v0.1.0/unix/zsyscall.go")).toBe(true);
    expect(isLibrary(null)).toBe(true);
    expect(isLibrary("/src/app.c")).toBe(false);
    expect(isLibrary("/home/me/project/src/library/main.rs")).toBe(false);
  });

  it("names the innermost function of the program's own code, past library code", () => {
    // get is the library's; step, inlined into scan, is the program's.
    expect(ownCodeAt(TREE.paths[1] ?? [], FILES)).toBe(1);
    expect(ownCodeAt(TREE.paths[0] ?? [], FILES)).toBe(0);
    expect(ownCodeAt(TREE.paths[6] ?? [], FILES)).toBe(0);
    // A function's own code has no bracket; library code alone is named
    // by the outermost function inlined.
    expect(inlinedFrom(TREE, FILES)).toEqual([null, 1, null, 1, 1, null, 2]);
  });
});

/** Entries as text, such as `line 1:14` for line 14 one level deep. */
function shown(entries: readonly SourceEntry[]): string[] {
  return entries.map((entry) => {
    switch (entry.kind) {
      case "line":
        return `line ${entry.depth}:${entry.line.line}`;
      case "text":
        return `text ${entry.depth}:${entry.number}`;
      case "gap":
        return `gap ${entry.depth}:${entry.count}`;
      default:
        return `call ${entry.depth}:${FUNCTIONS[entry.call.function]}`;
    }
  });
}

/** Lines without code from `from` to `to`, as `shown` reads them. */
function range(depth: number, from: number, to: number): string[] {
  return Array.from({ length: to - from + 1 }, (_, index) => `text ${depth}:${from + index}`);
}

describe("sourceEntries", () => {
  it("reads a function's lines as its file has them, with the text read around them", () => {
    expect(shown(sourceEntries(TREE, FILES, new Set()))).toEqual([
      ...range(0, 1, 2),
      "line 0:3",
      "line 0:4",
      ...range(0, 5, 7),
      "line 0:8",
      ...range(0, 9, 17),
      "line 0:18",
      ...range(0, 19, 30),
    ]);
  });

  it("opens what was inlined at a line beneath it, folding an inlined function's long stretches", () => {
    const four = lineAt([0, 4]);
    const fourteen = lineAt([0, 4], [1, 14]);
    const entries = shown(sourceEntries(TREE, FILES, new Set([four.key, fourteen.key])));
    expect(entries.slice(2, 10)).toEqual([
      "line 0:3",
      "line 0:4",
      "call 1:step",
      "line 1:14",
      "call 2:<core::option::Option<u8>>::get",
      // Its file cannot be read, so it is its line alone.
      "line 2:9",
      `gap 1:${FOLD}`,
      "line 1:19",
    ]);
    expect(entries[10]).toBe("text 0:5");
    // Line 8's step stays closed.
    expect(entries.filter((entry) => entry.startsWith("call"))).toHaveLength(2);
  });

  it("shows a short stretch of an inlined function rather than fold it", () => {
    const rows = codeRows(
      code([
        "f",
        [
          {
            ...instruction(0, "nop", 1),
            lines: [
              [1, 0, 20],
              [0, 0, 2],
            ],
          },
          {
            ...instruction(1, "nop", 1),
            lines: [
              [1, 0, 20 + FOLD],
              [0, 0, 2],
            ],
          },
        ],
      ]),
    );
    const tree = inlineTree(rows);
    const two = [...tree.lines.values()].find((line) => line.line === 2) as TreeLine;
    expect(shown(sourceEntries(tree, FILES, new Set([two.key])))).toEqual([
      "text 0:1",
      "line 0:2",
      "call 1:step",
      "line 1:20",
      ...range(1, 21, 19 + FOLD),
      `line 1:${20 + FOLD}`,
      ...range(0, 3, 30),
    ]);
  });
});

describe("tintsOf and tintOfPath", () => {
  it("tints the hottest lines, and under an open tinted line its hottest in shades of its hue", () => {
    const closed = tintsOf(TREE, new Set());
    // By the own weight under them: 4, 8, 3; 18 ran nothing.
    expect(closed.get(lineAt([0, 4]).key)).toBe(30);
    expect(closed.get(lineAt([0, 8]).key)).toBe(140);
    expect(closed.get(lineAt([0, 3]).key)).toBe(280);
    expect(closed.has(lineAt([0, 18]).key)).toBe(false);
    expect(closed.has(lineAt([0, 4], [1, 14]).key)).toBe(false);

    const open = tintsOf(TREE, new Set([lineAt([0, 4]).key]));
    expect(open.get(lineAt([0, 4], [1, 14]).key)).toBe(30);
    expect(open.get(lineAt([0, 4], [1, 19]).key)).toBe(58);
    // Not open, so not tinted.
    expect(open.has(lineAt([0, 8], [1, 14]).key)).toBe(false);
  });

  it("tints an instruction as its deepest line shown, or the nearest tinted one above", () => {
    const four = lineAt([0, 4]);
    const closed = new Set<string>();
    const open = new Set([four.key]);
    expect(tintOfPath(TREE.paths[1] ?? [], closed, tintsOf(TREE, closed))).toBe(30);
    expect(tintOfPath(TREE.paths[3] ?? [], open, tintsOf(TREE, open))).toBe(58);
    expect(tintOfPath(TREE.paths[3] ?? [], open, new Map([[four.key, 7]]))).toBe(7);
    expect(tintOfPath(TREE.paths[5] ?? [], closed, tintsOf(TREE, closed))).toBeUndefined();
    expect(tintOfPath([], closed, new Map())).toBeUndefined();
  });

  it("gives code of no source line no tint, though it is the hottest, nor its instructions", () => {
    const tree = inlineTree(
      codeRows(
        code([
          "f",
          [
            { ...instruction(0, "mov", 9), lines: [[0, 0, 0]] },
            { ...instruction(1, "add", 1), lines: [[0, 0, 5]] },
            // Inlined at line 5 with no line of its own.
            {
              ...instruction(2, "sub", 8),
              lines: [
                [1, 0, 0],
                [0, 0, 5],
              ],
            },
          ],
        ]),
      ),
    );
    const zero = [...tree.lines.values()].find((line) => line.line === 0 && !line.call.at);
    const five = [...tree.lines.values()].find((line) => line.line === 5) as TreeLine;
    const closed = tintsOf(tree, new Set());
    expect(closed.has(zero?.key ?? "")).toBe(false);
    // The hottest hue goes to the hottest line that has a number.
    expect(closed.get(five.key)).toBe(30);
    expect(tintOfPath(tree.paths[0] ?? [], new Set(), closed)).toBeUndefined();
    // Closed, line 5 holds the inlined code and tints it; open, its line 0 shows.
    expect(tintOfPath(tree.paths[2] ?? [], new Set(), closed)).toBe(30);
    const open = new Set([five.key]);
    expect(shownLine(tree.paths[2] ?? [], open)?.line).toBe(0);
    expect(tintOfPath(tree.paths[2] ?? [], open, tintsOf(tree, open))).toBeUndefined();
  });

  it("tints at most a handful of lines a level, none untouched, none under a closed line", () => {
    const chain = fc.array(
      fc.tuple(fc.integer({ min: 0, max: 3 }), fc.constant(0), fc.integer({ min: 1, max: 12 })),
      {
        minLength: 1,
        maxLength: 3,
      },
    );
    fc.assert(
      fc.property(
        fc.array(fc.tuple(chain, fc.nat(5)), { maxLength: 40 }),
        fc.array(fc.nat(), { maxLength: 6 }),
        (each, picks) => {
          const tree = inlineTree(
            codeRows(
              code([
                "f",
                each.map(([lines, own], index) => ({ ...instruction(index, "nop", own), lines })),
              ]),
            ),
          );
          const all = [...tree.lines.values()];
          const open = new Set(
            picks.flatMap((pick) => (all.length > 0 ? [all[pick % all.length]?.key ?? ""] : [])),
          );
          const tints = tintsOf(tree, open);
          const byLevel = new Map<string, number>();
          for (const line of all) {
            if (!tints.has(line.key)) {
              continue;
            }
            expect(line.inside + line.total).toBeGreaterThan(0);
            // Code of no source line is hatched, never tinted.
            expect(line.line).toBeGreaterThan(0);
            const above = line.call.at;
            // A line inlined somewhere is tinted only under an open, tinted line.
            expect(!above || (open.has(above.key) && tints.has(above.key))).toBe(true);
            const level = above?.key ?? "";
            byLevel.set(level, (byLevel.get(level) ?? 0) + 1);
          }
          for (const count of byLevel.values()) {
            expect(count).toBeLessThanOrEqual(TINTS);
          }
        },
      ),
    );
  });
});

describe("openingLine and linesAbove", () => {
  it("opens on the program's own hottest line, library code weighed where it was inlined", () => {
    // get's 5 count on step's line 14, which beats step's 3 at line 8.
    expect(openingLine(TREE, ROWS, FILES)).toBe(lineAt([0, 4], [1, 14]));
    // Read as the program's own, get's line is the hottest.
    const own = FILES.map((each) => ({ ...each, path: "/src/app.c" }));
    const get = lineAt([0, 4], [1, 14], [2, 9]);
    expect(openingLine(TREE, ROWS, own)).toBe(get);
    expect(linesAbove(get)).toEqual([lineAt([0, 4]).key, lineAt([0, 4], [1, 14]).key]);
    expect(linesAbove(lineAt([0, 3]))).toEqual([]);
  });

  it("opens an inlined function on its own hottest line", () => {
    expect(openingLine(TREE, ROWS, FILES, 1)).toBe(lineAt([0, 8], [1, 14]));
    expect(openingLine(inlineTree([]), [], FILES)).toBeUndefined();
  });
});

describe("runsOf", () => {
  it("covers exactly the rows given, each run as long as it can be", () => {
    fc.assert(
      fc.property(fc.uniqueArray(fc.nat(60), { maxLength: 40 }), (unsorted) => {
        const rows = unsorted.toSorted((a, b) => a - b);
        const runs = runsOf(rows);
        expect(
          runs.flatMap(([first, last]) =>
            Array.from({ length: last - first + 1 }, (_, at) => first + at),
          ),
        ).toEqual(rows);
        for (const [index, [, last]] of runs.entries()) {
          // No two runs touch, so each is as long as it can be.
          expect(runs[index + 1]?.[0] ?? Infinity).toBeGreaterThan(last + 1);
        }
      }),
    );
  });
});

describe("hottestRun", () => {
  const rows = codeRows(
    code([
      "scan",
      [
        instruction(0, "mov", 1, 1),
        instruction(1, "mov", 0, 0),
        instruction(2, "call", 0, 50),
        instruction(3, "mov", 0, 0),
        instruction(4, "add", 2, 2),
        instruction(5, "mov", 0, 0),
        instruction(6, "add", 2, 9),
      ],
    ]),
  );

  it("picks the run of most own, then of most total", () => {
    expect(
      hottestRun(rows, [
        [0, 0],
        [2, 2],
        [4, 4],
        [6, 6],
      ]),
    ).toBe(3);
    expect(
      hottestRun(rows, [
        [0, 1],
        [2, 2],
      ]),
    ).toBe(0);
    expect(
      hottestRun(rows, [
        [1, 1],
        [2, 2],
      ]),
    ).toBe(1);
    expect(hottestRun(rows, [])).toBe(0);
  });
});

describe("shortName", () => {
  it("keeps a function's own name and the type a method is of", () => {
    expect(
      shortName("<core::result::Result<&[u8; 8], core::array::TryFromSliceError>>::copied"),
    ).toBe("Result::copied");
    expect(
      shortName("<core::ops::range::Range<usize> as core::slice::index::SliceIndex<[u8]>>::index"),
    ).toBe("Range::index");
    expect(shortName("<u64 as core::slice::cmp::SliceContains>::slice_contains")).toBe(
      "u64::slice_contains",
    );
    expect(shortName("<[u8]>::is_empty")).toBe("[u8]::is_empty");
    expect(shortName("<microscope::perf::Fields>::u64")).toBe("Fields::u64");
    expect(shortName("microscope::backend::linux::perf::decode::next_with_period")).toBe(
      "next_with_period",
    );
    expect(shortName("field::<8>")).toBe("field");
    expect(shortName("scan")).toBe("scan");
    expect(shortName("operator<")).toBe("operator<");
  });
});
