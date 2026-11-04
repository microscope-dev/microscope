import { describe, expect, it } from "vitest";

import { best, score } from "../src/palette";

describe("palette matching", () => {
  it("ranks whole names, then beginnings, then parts, then scattered letters", () => {
    expect(score("step", "step")?.rank).toBe(0);
    expect(score("step over", "step")?.rank).toBe(1);
    expect(score("restart", "start")?.rank).toBe(2);
    expect(score("handle_request", "hreq")).toEqual({ rank: 3, marks: [0, 7, 8, 9] });
    expect(score("handle_request", "qh")).toBeNull();
    // Case is ignored, and marks name the characters matched.
    expect(score("Step Over", "over")).toEqual({ rank: 2, marks: [5, 6, 7, 8] });
  });

  it("keeps the best few, shorter names first among equals", () => {
    const names = ["table_insert", "table_find", "entry_set", "t"];
    expect(best(names, "t", (name) => name, 3)).toEqual(["t", "table_find", "table_insert"]);
    expect(best(names, "", (name) => name, 2)).toEqual(["table_insert", "table_find"]);
  });
});
