// The thread list: each thread with what its runtime says of it.

import { afterEach, expect, it } from "vitest";
import { page } from "vitest/browser";
import { Threads } from "../src/ui/Threads";
import { mountPane, stoppedState, unmountPane } from "./pane";

afterEach(unmountPane);

it("shows each thread's runtime labels beside its name", async () => {
  await mountPane(
    <Threads />,
    {},
    stoppedState({
      threads: [
        { id: 1, name: "node", stopped: true, labels: ["isolate 1"] },
        { id: 2, name: "libuv-worker", stopped: true, labels: [] },
        { id: 3, name: "alpha", stopped: true, labels: ["isolate 2"] },
      ],
    }),
  );
  await expect.element(page.getByTestId("threads")).toBeInTheDocument();
  const rows = page.getByTestId("threads").getByRole("button").elements();
  expect(rows.map((row) => row.textContent)).toEqual([
    "thread 1 (node)isolate 1◂ stopped here",
    "thread 2 (libuv-worker)stopped",
    "thread 3 (alpha)isolate 2stopped",
  ]);
});
