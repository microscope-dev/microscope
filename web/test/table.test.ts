// A card's Table and Copy CSV list every input value exactly as the
// renderer received it, one row per number, however many there are.

import { describe, expect, it } from "vitest";

import { csv, inputRows } from "../src/visualize/table";

describe("inputRows", () => {
  it("names each value by its path and writes it exactly", () => {
    const rows = inputRows({
      count: 3,
      big: 18446744073709551615n,
      ratio: 0.1,
      zero: -0,
      odd: new Float64Array([Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]),
      samples: new BigUint64Array([1n, 2n]),
      piece: { variant: "Some", value: { color: { name: "White", value: 0 } } },
      none: { variant: "None" },
      text: 'say "hi", then\nleave',
      flag: true,
      nothing: null,
      empty: [],
      bare: {},
      hits: [
        ["GET", 3],
        ["odd key", 4n],
      ],
      record: { "not a name": 1 },
    });
    const all = Array.from({ length: rows.count }, (_, index) => rows.row(index));
    expect(all).toEqual([
      ["count", "3"],
      ["big", "18446744073709551615"],
      ["ratio", "0.1"],
      ["zero", "-0"],
      ["odd[0]", "NaN"],
      ["odd[1]", "Infinity"],
      ["odd[2]", "-Infinity"],
      ["samples[0]", "1"],
      ["samples[1]", "2"],
      ["piece.variant", '"Some"'],
      ["piece.value.color.name", '"White"'],
      ["piece.value.color.value", "0"],
      ["none.variant", '"None"'],
      ["text", '"say \\"hi\\", then\\nleave"'],
      ["flag", "true"],
      ["nothing", "null"],
      ["empty", "[]"],
      ["bare", "{}"],
      ["hits[0][0]", '"GET"'],
      ["hits[0][1]", "3"],
      ["hits[1][0]", '"odd key"'],
      ["hits[1][1]", "4"],
      ['record["not a name"]', "1"],
    ]);
  });

  it("writes a 32-bit float as the shortest text that reads back as it", () => {
    const rows = inputRows({ f: new Float32Array([0.1, 1 / 3, 16777217, -0, Number.NaN]) });
    expect(Array.from({ length: rows.count }, (_, index) => rows.row(index)[1])).toEqual([
      "0.1",
      "0.33333334",
      "16777216",
      "-0",
      "NaN",
    ]);
  });

  it("reads a million numbers a row at a time, without listing them", () => {
    const values = new Float32Array(1_000_000).map((_, index) => index / 4);
    const rows = inputRows({ before: 1, values, after: 2 });
    expect(rows.count).toBe(1_000_002);
    expect(rows.row(0)).toEqual(["before", "1"]);
    expect(rows.row(1)).toEqual(["values[0]", "0"]);
    expect(rows.row(999_999)).toEqual(["values[999998]", "249999.5"]);
    expect(rows.row(1_000_001)).toEqual(["after", "2"]);
  });
});

describe("csv", () => {
  it("quotes only what needs it and writes strings as their text", () => {
    expect(
      csv(inputRows({ name: 'a "b", c', n: 2n, label: "plain", lines: "x\ny", values: [1.5] })),
    ).toBe('path,value\nname,"a ""b"", c"\nn,2\nlabel,plain\nlines,"x\ny"\nvalues[0],1.5\n');
  });
});
