// A link can name a stop the session has passed or one it has not reached
// yet, and every pane says which.

import { afterEach, expect, it } from "vitest";
import { page } from "vitest/browser";

import type { StopEntry } from "../src/protocol";
import { Registers } from "../src/ui/Registers";
import { Stack } from "../src/ui/Stack";
import { Variables } from "../src/ui/Values";
import { mountPane, stoppedState, unmountPane } from "./pane";

afterEach(unmountPane);

const entry = (stop: number): StopEntry => ({
  stop,
  thread: 1,
  reason: { kind: "breakpoint", description: "breakpoint 1" },
  frame: 0,
  place: null,
  by: null,
  action: null,
});

const state = stoppedState({ stops: [entry(1), entry(2)] });

it.each([
  [<Stack key="stack" />, "This session has not reached stop #9."],
  [<Variables key="variables" />, "Values hidden: the session has not reached stop #9."],
  [<Registers key="registers" />, "Registers hidden: the session has not reached stop #9."],
])("says a stop not reached yet has not been reached", async (pane, text) => {
  await mountPane(pane, { at: { stop: 9, thread: 1, frame: 0 }, stale: "passed" }, state);
  await expect.element(page.getByText(text)).toBeVisible();
  expect(document.body.textContent).not.toContain("has passed");
});

it("says a stop the session has left has passed", async () => {
  await mountPane(<Stack />, { at: { stop: 1, thread: 1, frame: 0 }, stale: "passed" }, state);
  await expect
    .element(page.getByText("Stop #1 has passed; its stack is gone with it."))
    .toBeVisible();
});
