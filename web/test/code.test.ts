// How a function's instructions map to the lines of a source file: which
// line each belongs to, where its basic blocks begin, which lines get a
// tint, and how a line's instructions fall into runs.

import * as fc from "fast-check";
import { describe, expect, it } from "vitest";

import {
  type CodeRow,
  codeRows,
  hottestRun,
  lineIn,
  linesOf,
  openingLine,
  runsOf,
  shortName,
  TINTS,
  tintsOf,
} from "../src/profile/code";
import type { ProfileCode, ProfileInstruction, ProfileSourceFile } from "../src/protocol";

function instruction(address: number, text: string, own = 0, total = own): ProfileInstruction {
  return { address, text, target: null, own, total, lines: [] };
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

function file(lines: [number, number, number][]): ProfileSourceFile {
  return { path: "/src/app.c", lines, first: 1, text: null, unread: null };
}

describe("lineIn", () => {
  it("takes the innermost line in the file asked for, not the innermost overall", () => {
    // `get` from h.h (file 1) inlined into `step` in app.c (file 0),
    // inlined into `scan` in app.c.
    const lines: [number, number | null, number][] = [
      [0, 1, 3],
      [1, 0, 8],
      [2, 0, 14],
    ];
    expect(lineIn(lines, 0)).toBe(8);
    expect(lineIn(lines, 1)).toBe(3);
    expect(lineIn(lines, 2)).toBe(0);
  });

  it("passes over lines the debug information does not name", () => {
    expect(
      lineIn(
        [
          [0, 0, 0],
          [1, 0, 14],
        ],
        0,
      ),
    ).toBe(14);
    expect(lineIn([[0, null, 9]], 0)).toBe(0);
    expect(lineIn([], 0)).toBe(0);
  });
});

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

describe("runsOf", () => {
  it("splits a line's instructions where others come between", () => {
    expect(runsOf([3, 3, 5, 3, 0, 0, 3], 3)).toEqual([
      [0, 1],
      [3, 3],
      [6, 6],
    ]);
    expect(runsOf([3, 3], 4)).toEqual([]);
  });

  it("covers exactly the line's instructions, in order, each run as long as it can be", () => {
    fc.assert(
      fc.property(
        fc.array(fc.integer({ min: 0, max: 4 }), { maxLength: 60 }),
        fc.integer({ min: 0, max: 4 }),
        (lines, line) => {
          const runs = runsOf(lines, line);
          const covered = runs.flatMap(([first, last]) =>
            Array.from({ length: last - first + 1 }, (_, at) => first + at),
          );
          const expected = lines.flatMap((each, index) => (each === line ? [index] : []));
          expect(covered).toEqual(expected);
          for (const [first, last] of runs) {
            expect(first).toBeLessThanOrEqual(last);
            // Maximal: something else comes just before and just after,
            // so no two runs touch.
            expect(lines[first - 1]).not.toBe(line);
            expect(lines[last + 1]).not.toBe(line);
          }
        },
      ),
    );
  });
});

describe("tintsOf", () => {
  it("tints the hottest lines by own, then total, then the earlier line", () => {
    const tints = tintsOf(
      file([
        [10, 0, 90],
        [11, 5, 5],
        [12, 5, 9],
        [13, 0, 0],
        [14, 1, 1],
        [9, 1, 1],
      ]),
      4,
    );
    expect([...tints]).toEqual([
      [12, 0],
      [11, 1],
      [9, 2],
      [14, 3],
    ]);
    // A call site whose cost is all in its callees is past the fourth.
    expect(tints.has(10)).toBe(false);
    expect(tints.has(13)).toBe(false);
  });

  const line = fc.tuple(
    fc.integer({ min: 1, max: 40 }),
    fc.integer({ min: 0, max: 5 }),
    fc.integer({ min: 0, max: 5 }),
  );
  const lines = fc.uniqueArray(line, { selector: ([at]) => at, maxLength: 20 });

  it("tints at most a handful of lines, each a different tint, none untouched", () => {
    fc.assert(
      fc.property(lines, (each) => {
        const tints = tintsOf(file(each));
        expect(tints.size).toBeLessThanOrEqual(TINTS);
        expect(new Set(tints.values()).size).toBe(tints.size);
        for (const [at] of tints) {
          const [, own, total] = each.find(([number]) => number === at) ?? [0, 0, 0];
          expect(own + total).toBeGreaterThan(0);
        }
      }),
    );
  });

  it("never leaves a line hotter than a tinted one untinted", () => {
    fc.assert(
      fc.property(lines, (each) => {
        const tints = tintsOf(file(each));
        const weighed = new Map(each.map(([at, own, total]) => [at, [own, total]]));
        const hotter = (a: number, b: number) => {
          const [ownA = 0, totalA = 0] = weighed.get(a) ?? [];
          const [ownB = 0, totalB = 0] = weighed.get(b) ?? [];
          return (
            ownA > ownB || (ownA === ownB && (totalA > totalB || (totalA === totalB && a < b)))
          );
        };
        for (const [at, own, total] of each) {
          if (tints.has(at) || own + total === 0) {
            continue;
          }
          for (const tinted of tints.keys()) {
            expect(hotter(tinted, at)).toBe(true);
          }
        }
        // Tints run in order of heat.
        for (const [a, tintA] of tints) {
          for (const [b, tintB] of tints) {
            expect(tintA < tintB).toBe(tintA !== tintB && hotter(a, b));
          }
        }
      }),
    );
  });
});

describe("hottestRun and openingLine", () => {
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
    // Own 1, own 0 with 50 total, then own 2 twice: the second has more total.
    expect(
      hottestRun(rows, [
        [0, 0],
        [2, 2],
        [4, 4],
        [6, 6],
      ]),
    ).toBe(3);
    // Total breaks no tie of own when the first is hotter.
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

  it("opens on the file's hottest line, or the first line any instruction has", () => {
    expect(
      openingLine(
        file([
          [7, 0, 40],
          [8, 3, 3],
        ]),
        [7, 8],
      ),
    ).toBe(8);
    expect(openingLine(undefined, [0, 0, 12, 4])).toBe(12);
    // An inlined function opens on its own hottest line, not its caller's.
    expect(
      openingLine(
        file([
          [7, 0, 40],
          [8, 3, 3],
          [9, 1, 1],
        ]),
        [7, 8, 9],
        new Set([7, 9]),
      ),
    ).toBe(9);
    expect(openingLine(file([[3, 0, 0]]), [0])).toBe(0);
  });
});

describe("shortName", () => {
  it("keeps only a function's own name", () => {
    expect(
      shortName("<core::result::Result<&[u8; 8], core::array::TryFromSliceError>>::copied"),
    ).toBe("copied");
    expect(shortName("microscope::backend::linux::perf::decode::next_with_period")).toBe(
      "next_with_period",
    );
    expect(shortName("field::<8>")).toBe("field");
    expect(shortName("scan")).toBe("scan");
    expect(shortName("operator<")).toBe("operator<");
  });
});

describe("linesOf", () => {
  it("finds a function's lines in one file wherever it was inlined", () => {
    const rows = codeRows(
      code([
        "scan",
        [
          {
            ...instruction(0, "mov"),
            lines: [
              [1, 0, 8],
              [0, 0, 14],
            ],
          },
          {
            ...instruction(1, "add"),
            lines: [
              [2, 1, 3],
              [1, 0, 9],
              [0, 0, 14],
            ],
          },
          { ...instruction(2, "ret"), lines: [[0, 0, 15]] },
          {
            ...instruction(3, "nop"),
            lines: [
              [1, 1, 30],
              [0, 0, 0],
            ],
          },
        ],
      ]),
    );
    expect([...linesOf(rows, 1, 0)].toSorted((a, b) => a - b)).toEqual([8, 9]);
    expect([...linesOf(rows, 1, 1)]).toEqual([30]);
    expect([...linesOf(rows, 0, 0)].toSorted((a, b) => a - b)).toEqual([14, 15]);
    expect(linesOf(rows, 5, 0).size).toBe(0);
  });
});
