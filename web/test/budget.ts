import { afterEach, beforeEach, expect, inject } from "vitest";

const UNIT = 150;

const BROWSER = 1_500;

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

const searching = inject("proptestCases") !== "";

const scale = inject("budgetScale");

afterEach(() => {
  if (searching) {
    return;
  }
  const elapsed = performance.now() - started;
  const { testPath, currentTestName } = expect.getState();
  const file = (testPath ?? "").replace(/^.*\/web\//u, "");
  const name = `${file} :: ${currentTestName ?? ""}`;
  const own = OWN[name] ?? FILES[file];
  const base = own?.ms ?? (typeof window === "undefined" ? UNIT : BROWSER);
  const budget = Math.round(scale * base);
  if (elapsed > budget) {
    const scaled = scale === 1 ? "" : ` (${base}ms times MICROSCOPE_TEST_BUDGET_SCALE ${scale})`;
    throw new Error(
      `${name} took ${Math.round(elapsed)}ms, over its ${budget}ms budget${scaled}. ` +
        "Make it faster, or give it a budget of its own in test/budget.ts with the reason.",
    );
  }
});
