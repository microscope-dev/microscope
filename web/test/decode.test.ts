// A drawing's inputs come out of its binary frame exactly, or not at all.

import { describe, expect, it } from "vitest";

import type { DrawInput } from "../src/protocol";
import { DecodeError, decodeInputs } from "../src/visualize/decode";

const input = (value: DrawInput["value"]): DrawInput[] => [{ name: "samples", value, path: null }];

describe("decodeInputs", () => {
  it("copies numbers and bytes out of the frame", () => {
    const frame = new Uint8Array(new Float64Array([1.5, -2]).buffer);
    expect(decodeInputs(input({ t: "numbers", kind: "f64", offset: 0, count: 2 }), frame)).toEqual({
      samples: new Float64Array([1.5, -2]),
    });
    expect(decodeInputs(input({ t: "bytes", offset: 8, length: 2 }), frame)).toEqual({
      samples: frame.slice(8, 10),
    });
  });

  it("refuses numbers or bytes that lie past the frame, naming the input", () => {
    const frame = new Uint8Array(12);
    // Twelve bytes hold one f64 and half of another.
    expect(() =>
      decodeInputs(input({ t: "numbers", kind: "f64", offset: 0, count: 2 }), frame),
    ).toThrow(new DecodeError("samples: 2 f64 numbers at byte 0 lie past the 12 bytes sent"));
    expect(() => decodeInputs(input({ t: "bytes", offset: 10, length: 4 }), frame)).toThrow(
      new DecodeError("samples: 4 bytes at byte 10 lie past the 12 bytes sent"),
    );
  });

  it("refuses a count that is not whole, or a kind it does not know", () => {
    const frame = new Uint8Array(8);
    // One and a half i16 numbers are three whole bytes.
    const half = input({ t: "numbers", kind: "i16", offset: 0, count: 1.5 });
    expect(() => decodeInputs(half, frame)).toThrow(DecodeError);
    expect(() => decodeInputs(half, frame)).toThrow("samples: ");
    for (const kind of ["f16", "toString"]) {
      const unknown = input({ t: "numbers", kind, offset: 0, count: 1 } as DrawInput["value"]);
      expect(() => decodeInputs(unknown, frame)).toThrow(DecodeError);
      expect(() => decodeInputs(unknown, frame)).toThrow("samples: ");
    }
  });
});
