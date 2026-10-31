// Memory as the page shows it: rows of sixteen bytes, and a selection read
// as the types its length could be. Addresses are bigints, since a 64-bit
// address does not fit a number. The target is little-endian, as x86-64 is.

import { assert } from "./assert";

/** Bytes shown in each row. */
export const ROW = 16;

/** What `mem=` names: an address, and the bytes of the value there. */
export interface Target {
  address: bigint;
  bytes: number | null;
}

/** Parses `0xADDRESS` or `0xADDRESS:BYTES`. */
export function target(text: string): Target | null {
  const match = /^(0x[0-9a-f]{1,16})(?::(\d+))?$/i.exec(text);
  if (!match) {
    return null;
  }
  const bytes = match[2] === undefined ? null : Number(match[2]);
  if (bytes === 0) {
    return null;
  }
  return { address: BigInt(match[1] as string), bytes };
}

/** A target as a link writes it. */
export function targetText(target: Target): string {
  return target.bytes ? `${hex(target.address)}:${target.bytes}` : hex(target.address);
}

export function hex(address: bigint): string {
  return `0x${address.toString(16)}`;
}

/** The start of the row holding `address`. */
export function rowStart(address: bigint): bigint {
  assert(address >= 0n, `row of negative address ${address}`);
  return address - (address % BigInt(ROW));
}

export interface DumpRow {
  address: bigint;
  /** Each byte, or null where nothing was read. */
  bytes: (number | null)[];
}

/**
 * `count` rows from the row holding `start`, filled with the bytes read at
 * `start`, given as hexadecimal pairs.
 */
export function dumpRows(start: bigint, read: string, count: number): DumpRow[] {
  assert(Number.isInteger(count) && count >= 0, `dumping ${count} rows`);
  const first = rowStart(start);
  const offset = Number(start - first);
  assert(offset >= 0 && offset < ROW, `address ${hex(start)} is ${offset} bytes into its row`);
  const rows: DumpRow[] = [];
  for (let row = 0; row < count; row++) {
    const bytes: (number | null)[] = [];
    for (let column = 0; column < ROW; column++) {
      const index = row * ROW + column - offset;
      const pair = index >= 0 ? read.slice(index * 2, index * 2 + 2) : "";
      bytes.push(pair.length === 2 ? Number.parseInt(pair, 16) : null);
    }
    rows.push({ address: first + BigInt(row * ROW), bytes });
  }
  return rows;
}

/** How a byte reads as text in the dump's right column. */
export function printable(byte: number | null): string {
  if (byte === null) {
    return " ";
  }
  return byte >= 0x20 && byte < 0x7f ? String.fromCharCode(byte) : ".";
}

export interface Reading {
  as: string;
  text: string;
}

/** The selection read as integers, a float, and text, as its length allows. */
export function readAs(bytes: readonly number[]): Reading[] {
  const readings: Reading[] = [];
  const width = bytes.length;
  if ([1, 2, 4, 8].includes(width)) {
    let unsigned = 0n;
    for (const [index, byte] of bytes.entries()) {
      unsigned |= BigInt(byte) << BigInt(index * 8);
    }
    const bits = BigInt(width * 8);
    const signed = BigInt.asIntN(Number(bits), unsigned);
    readings.push(
      { as: `u${bits}`, text: unsigned.toString() },
      { as: `i${bits}`, text: signed.toString() },
      { as: "hex", text: `0x${unsigned.toString(16)}` },
    );
    const view = new DataView(Uint8Array.from(bytes).buffer);
    if (width === 4) {
      readings.push({ as: "f32", text: float(view.getFloat32(0, true), 6) });
    } else if (width === 8) {
      readings.push({ as: "f64", text: float(view.getFloat64(0, true), 15) });
    }
  }
  readings.push({ as: "text", text: quoted(bytes) });
  return readings;
}

/** A float to the digits its type holds, without trailing zeros. */
function float(value: number, digits: number): string {
  return Number.isFinite(value) ? String(Number(value.toPrecision(digits))) : String(value);
}

/** Bytes as a C string literal: printable ASCII as itself, the rest escaped. */
function quoted(bytes: readonly number[]): string {
  const escapes: Record<number, string> = { 0: "\\0", 9: "\\t", 10: "\\n", 13: "\\r", 34: '\\"' };
  let text = "";
  for (const byte of bytes) {
    text +=
      escapes[byte] ??
      (byte >= 0x20 && byte < 0x7f && byte !== 0x5c
        ? String.fromCharCode(byte)
        : byte === 0x5c
          ? "\\\\"
          : `\\x${byte.toString(16).padStart(2, "0")}`);
  }
  return `"${text}"`;
}
