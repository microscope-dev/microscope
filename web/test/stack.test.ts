// A call stack folds its runtime's machinery into a row per run, and shows
// a run that holds the frame chosen.

import { describe, expect, it } from "vitest";
import type { Backtrace, Frame } from "../src/protocol";
import { type StackRow, stackRows } from "../src/stack";

const frame = (index: number): Frame => ({
  index,
  name: `f${index}`,
  kind: "physical",
  address: null,
  module: null,
  source: null,
  language: null,
  unfollowed: null,
});

const trace: Backtrace = {
  frames: [0, 1, 2, 3, 4, 5, 6].map(frame),
  folds: [
    { first: 1, count: 2 },
    { first: 4, count: 3 },
  ],
  incomplete: null,
};

/** Each row as a word: a frame's index, or a run's first frame and
 * whether it is open. */
const shown = (rows: StackRow[]) =>
  rows.map((row) =>
    row.kind === "frame"
      ? `${row.frame.index}`
      : `fold ${row.run.first} ${row.open ? "open" : "shut"}`,
  );

describe("a call stack's rows", () => {
  it("fold each run of machinery into one row", () => {
    expect(shown(stackRows(trace, 0, new Set()))).toEqual(["0", "fold 1 shut", "3", "fold 4 shut"]);
  });

  it("show the frames of a run opened", () => {
    expect(shown(stackRows(trace, 0, new Set([4])))).toEqual([
      "0",
      "fold 1 shut",
      "3",
      "fold 4 open",
      "4",
      "5",
      "6",
    ]);
  });

  it("show a run that holds the frame chosen", () => {
    expect(shown(stackRows(trace, 2, new Set()))).toEqual([
      "0",
      "fold 1 open",
      "1",
      "2",
      "3",
      "fold 4 shut",
    ]);
  });

  it("are the frames alone where nothing folds", () => {
    expect(shown(stackRows({ ...trace, folds: [] }, undefined, new Set()))).toEqual([
      "0",
      "1",
      "2",
      "3",
      "4",
      "5",
      "6",
    ]);
  });
});
