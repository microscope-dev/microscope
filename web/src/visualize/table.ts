// A drawing's inputs as rows of a path and a value, for a card's Table and
// Copy CSV. Each number is written exactly: a bigint as its whole integer,
// a float as the shortest text that reads back as the same float. Arrays
// of numbers are read a row at a time, so a million samples cost nothing
// until a row is shown.

import { assert } from "../assert";
import type { TypedArray, Value } from "./decode";

/** Every value of a drawing's inputs, one row each. */
export interface InputRows {
  count: number;
  /** The row's path and its value as the Table shows it, strings quoted. */
  row(index: number): [string, string];
  /** The row's path and its value as CSV holds it, strings as their text. */
  cell(index: number): [string, string];
}

/** A run of rows: one value, an empty list or record, or a typed array's
 * elements. */
type Run =
  | { path: string; value: null | boolean | number | bigint | string }
  | { path: string; empty: "[]" | "{}" }
  | { path: string; array: TypedArray };

const NAME = /^[A-Za-z_$][\w$]*$/;

export function inputRows(inputs: Record<string, Value>): InputRows {
  const runs: Run[] = [];
  for (const [name, value] of Object.entries(inputs)) {
    collect(value, name, runs);
  }
  // Where each run starts, for finding a row's run by bisection.
  const starts: number[] = [];
  let count = 0;
  for (const run of runs) {
    starts.push(count);
    count += "array" in run ? run.array.length : 1;
  }
  const at = (index: number, quote: boolean): [string, string] => {
    assert(Number.isInteger(index), "row index is whole");
    assert(index >= 0 && index < count, "row index is within the rows");
    let low = 0;
    let high = runs.length - 1;
    while (low < high) {
      const middle = (low + high + 1) >> 1;
      if ((starts[middle] as number) <= index) {
        low = middle;
      } else {
        high = middle - 1;
      }
    }
    const run = runs[low] as Run;
    assert((starts[low] as number) <= index, "row's run starts at or before it");
    if ("array" in run) {
      const offset = index - (starts[low] as number);
      assert(offset < run.array.length, "row lies within its run's array");
      const value = run.array[offset] as number | bigint;
      return [
        `${run.path}[${offset}]`,
        run.array instanceof Float32Array ? float32(value as number) : text(value),
      ];
    }
    assert(index === starts[low], "a single value's row is its run's start");
    if ("empty" in run) {
      return [run.path, run.empty];
    }
    const { value } = run;
    return [
      run.path,
      typeof value === "string" ? (quote ? JSON.stringify(value) : value) : text(value),
    ];
  };
  return {
    count,
    row: (index) => at(index, true),
    cell: (index) => at(index, false),
  };
}

function text(value: null | boolean | number | bigint): string {
  if (typeof value === "number" && Object.is(value, -0)) {
    return "-0";
  }
  return String(value);
}

/** A 32-bit float as the shortest text that reads back as the same one. */
function float32(value: number): string {
  if (Number.isFinite(value) && !Object.is(value, -0)) {
    for (let digits = 1; digits <= 9; digits++) {
      const shortest = Number(value.toPrecision(digits));
      if (Math.fround(shortest) === value) {
        return String(shortest);
      }
    }
    assert(false, "9 digits round-trip every finite 32-bit float");
  }
  return text(value);
}

function collect(value: Value, path: string, runs: Run[]): void {
  if (ArrayBuffer.isView(value)) {
    runs.push(
      (value as TypedArray).length === 0
        ? { path, empty: "[]" }
        : { path, array: value as TypedArray },
    );
  } else if (Array.isArray(value)) {
    if (value.length === 0) {
      runs.push({ path, empty: "[]" });
    }
    value.forEach((item, index) => {
      collect(item, `${path}[${index}]`, runs);
    });
  } else if (typeof value === "object" && value !== null) {
    const members = Object.entries(value);
    if (members.length === 0) {
      runs.push({ path, empty: "{}" });
    }
    for (const [name, member] of members) {
      collect(
        member,
        NAME.test(name) ? `${path}.${name}` : `${path}[${JSON.stringify(name)}]`,
        runs,
      );
    }
  } else {
    runs.push({ path, value });
  }
}

/** The rows as CSV, a header and then one line each. */
export function csv(rows: InputRows): string {
  const lines = ["path,value"];
  for (let index = 0; index < rows.count; index++) {
    lines.push(rows.cell(index).map(field).join(","));
  }
  return `${lines.join("\n")}\n`;
}

function field(value: string): string {
  return /[",\n\r]/.test(value) ? `"${value.replaceAll('"', '""')}"` : value;
}
