// How long one test may take. The suite runs on every save, so a slow test
// is a defect: make it faster, or give it a budget of its own below with the
// reason it cannot be faster. A test in a browser waits on real key presses,
// garbage collection, and modules loading cold, so its budget leaves room
// for that. Budgets fit jlinux3, the slowest machine we run on, where tests
// take about twice as long as on jlinux1. A budget of a test's own is about
// half again its time. `just web-props` runs without budgets.

import { afterEach, beforeEach, expect, inject } from "vitest";

/** Tests outside a browser: logic over data. */
const UNIT = 150;

/** Tests rendering in Chromium. */
const BROWSER = 1_500;

/** Tests that cannot meet their project's budget, by file and full name. */
export const OWN: Record<string, { ms: number; why: string }> = {
  "test/live.test.tsx :: a live renderer > is ended when a function it returned never settles": {
    ms: 4_500,
    why: "waits out the live renderer's real 3s deadline",
  },
  "test/live.test.tsx :: a live renderer > is ended when it stops answering, and starts afresh": {
    ms: 4_500,
    why: "waits out the live renderer's real 3s deadline",
  },
  "test/sandbox.test.tsx :: the sandbox > ends a draw that runs too long, and the card draws again afterwards":
    { ms: 3_000, why: "waits out the sandbox's real 2s draw deadline" },
  "test/path-input.test.tsx :: passes Tab through an empty field and stops descending at a directory":
    { ms: 2_000, why: "types paths a key at a time through the browser" },
  "test/path-input.test.tsx :: never completes typed text with suggestions for older text": {
    ms: 2_000,
    why: "types paths a key at a time through the browser",
  },
  "test/docking.test.tsx :: keeps the screen and the layout model alike through any run of drags and clicks":
    { ms: 4_500, why: "a hundred runs of up to a dozen drags and clicks, each checked on screen" },
  "test/documents.test.ts :: arranging the documents > opens a Source pane for each file, in order, and shows what the address names":
    {
      ms: 350,
      why: "builds a hundred layouts of up to 40 checked operations, first in its file, so it warms the layout code; up to 220ms in the full suite",
    },
  "test/sandbox.test.tsx :: the sandbox > runs four draws at once and queues the rest": {
    ms: 2_500,
    why: "holds four real worker draws open while a fifth queues",
  },
};

/** Files whose every test has a budget of its own, and why. */
export const FILES: Record<string, { ms: number; why: string }> = {
  "test/journeys.test.tsx": {
    ms: 3_500,
    why: "a journey is several steps of a person's, each rendering the whole page",
  },
};

let started = 0;

beforeEach(() => {
  started = performance.now();
});

// A deeper property search is meant to take its time.
const searching = inject("proptestCases") !== "";

afterEach(() => {
  if (searching) {
    return;
  }
  const elapsed = performance.now() - started;
  const { testPath, currentTestName } = expect.getState();
  const file = (testPath ?? "").replace(/^.*\/web\//u, "");
  const name = `${file} :: ${currentTestName ?? ""}`;
  const own = OWN[name] ?? FILES[file];
  const budget = own?.ms ?? (typeof window === "undefined" ? UNIT : BROWSER);
  if (elapsed > budget) {
    throw new Error(
      `${name} took ${Math.round(elapsed)}ms, over its ${budget}ms budget. ` +
        "Make it faster, or give it a budget of its own in test/budget.ts with the reason.",
    );
  }
});
