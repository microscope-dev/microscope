// A drawing's inputs as its renderer receives them: plain JS values, each
// decided by its type alone (docs/visualizers.md). Numbers and bytes are
// copied out of the drawing's binary frame into arrays of their own, so
// that each crosses into the sandbox without the rest.

import type { Datum, DrawInput, NumberKind } from "../protocol";

/** A value as a renderer receives it. */
export type Value =
  | null
  | boolean
  | number
  | bigint
  | string
  | Value[]
  | TypedArray
  | { [member: string]: Value };

export type TypedArray =
  | Int8Array
  | Uint8Array
  | Int16Array
  | Uint16Array
  | Int32Array
  | Uint32Array
  | BigInt64Array
  | BigUint64Array
  | Float32Array
  | Float64Array;

/** Why an input could not be read from the drawing's binary frame. */
export class DecodeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "DecodeError";
  }
}

const ARRAYS = {
  i8: Int8Array,
  u8: Uint8Array,
  i16: Int16Array,
  u16: Uint16Array,
  i32: Int32Array,
  u32: Uint32Array,
  i64: BigInt64Array,
  u64: BigUint64Array,
  f32: Float32Array,
  f64: Float64Array,
} as const satisfies Record<NumberKind, unknown>;

/** The inputs by name, with `bytes` the drawing's binary frame's payload. */
export function decodeInputs(inputs: DrawInput[], bytes: Uint8Array): Record<string, Value> {
  return Object.fromEntries(
    inputs.map((input) => {
      try {
        return [input.name, decode(input.value, bytes)];
      } catch (error) {
        throw error instanceof DecodeError
          ? new DecodeError(`${input.name}: ${error.message}`)
          : error;
      }
    }),
  );
}

/** Fails unless `length` bytes from `offset` lie in `bytes`: a short frame
 * would otherwise give a short array, or a RangeError. */
function within(bytes: Uint8Array, offset: number, length: number, what: string): void {
  if (
    !Number.isSafeInteger(offset) ||
    !Number.isSafeInteger(length) ||
    offset < 0 ||
    length < 0 ||
    offset + length > bytes.length
  ) {
    throw new DecodeError(`${what} at byte ${offset} lie past the ${bytes.length} bytes sent`);
  }
}

/** The part of the drawn value each input is, by name, or null. */
export function inputPaths(inputs: DrawInput[]): Record<string, string | null> {
  return Object.fromEntries(inputs.map((input) => [input.name, input.path]));
}

/** One input, whose numbers and bytes lie in `bytes`. */
export function decode(datum: Datum, bytes: Uint8Array): Value {
  switch (datum.t) {
    case "bool":
      return datum.b;
    case "int":
      return datum.i;
    case "big":
      return BigInt(datum.big);
    case "float":
      return Number(datum.f);
    case "text":
      return datum.s;
    case "enum":
      return { name: datum.name, value: decode(datum.value, bytes) };
    case "sum":
      return datum.value === null
        ? { variant: datum.variant }
        : { variant: datum.variant, value: decode(datum.value, bytes) };
    case "record":
      return Object.fromEntries(
        datum.members.map(([name, member]) => [name, decode(member, bytes)]),
      );
    case "list":
      return datum.items.map((item) => decode(item, bytes));
    case "entries":
      return datum.entries.map(([key, value]) => [decode(key, bytes), decode(value, bytes)]);
    case "numbers": {
      const Kind = ARRAYS[datum.kind];
      const length = datum.count * Kind.BYTES_PER_ELEMENT;
      within(bytes, datum.offset, length, `${datum.count} ${datum.kind} numbers`);
      const start = bytes.byteOffset + datum.offset;
      return new Kind(bytes.buffer.slice(start, start + length) as ArrayBuffer);
    }
    case "bytes":
      within(bytes, datum.offset, datum.length, `${datum.length} bytes`);
      return bytes.slice(datum.offset, datum.offset + datum.length);
    case "null":
      return null;
  }
}
