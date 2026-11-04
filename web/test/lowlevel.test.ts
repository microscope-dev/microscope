import { describe, expect, it } from "vitest";

import { dumpRows, readAs, target, targetText } from "../src/memory";

describe("memory", () => {
  it("names an address and how many bytes the value there occupies", () => {
    expect(target("0x7ffff7a3e020:16")).toEqual({ address: 0x7ffff7a3e020n, bytes: 16 });
    expect(target("0xffffffffff600000")).toEqual({ address: 0xffffffffff600000n, bytes: null });
    expect(target("0x10:0")).toBeNull();
    // A byte count past what a link may name is malformed.
    expect(target("0x10:999999999")).toEqual({ address: 0x10n, bytes: 999999999 });
    expect(target("0x10:1000000000")).toBeNull();
    expect(target(`0x10:${"9".repeat(400)}`)).toBeNull();
    expect(target("main")).toBeNull();
    // As a link writes it.
    expect(targetText({ address: 0x1fn, bytes: 4 })).toBe("0x1f:4");
    expect(targetText({ address: 0x1fn, bytes: null })).toBe("0x1f");
  });

  it("lays bytes out sixteen to a row, from the row holding the address", () => {
    const rows = dumpRows(0x1008n, "41424300", 2);
    expect(rows.map((row) => row.address)).toEqual([0x1000n, 0x1010n]);
    // Bytes before the read and past its end are not shown as values.
    expect(rows[0]?.bytes.slice(6, 13)).toEqual([null, null, 0x41, 0x42, 0x43, 0x00, null]);
  });

  it("reads a selection as the types its length could be", () => {
    const user = [0x75, 0x73, 0x65, 0x72];
    expect(readAs(user)).toEqual([
      { as: "u32", text: "1919251317" },
      { as: "i32", text: "1919251317" },
      { as: "hex", text: "0x72657375" },
      { as: "f32", text: "4.54475e+30" },
      { as: "text", text: '"user"' },
    ]);
    const minusOne = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
    expect(readAs(minusOne).slice(0, 3)).toEqual([
      { as: "u64", text: "18446744073709551615" },
      { as: "i64", text: "-1" },
      { as: "hex", text: "0xffffffffffffffff" },
    ]);
    // Lengths no integer has read only as text.
    expect(readAs([0x61, 0x00, 0x0a])).toEqual([{ as: "text", text: '"a\\0\\n"' }]);
  });
});
