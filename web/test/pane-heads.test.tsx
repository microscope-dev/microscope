// A pane's heading stays one line, however long the name it shows.

import { cleanup } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { page } from "vitest/browser";
import "../src/styles.css";
import { Stack } from "../src/ui/Stack";
import { mountPane, stoppedState } from "./pane";

afterEach(cleanup);

it("cuts a long thread name short rather than wrapping the call stack's heading", async () => {
  const name = "a-worker-thread-with-a-very-long-name-indeed";
  await mountPane(
    <div style={{ width: "260px", display: "flex", flexDirection: "column" }}>
      <Stack />
    </div>,
    {},
    stoppedState({ threads: [{ id: 1, name, stopped: true, labels: [] }] }),
  );
  await expect.element(page.getByText(name, { exact: false })).toBeInTheDocument();
  const head = document.querySelector('[aria-label="Call stack"] .pane-head') as HTMLElement;
  const label = head.querySelector(".count") as HTMLElement;
  expect(head.scrollHeight).toBe(head.clientHeight);
  expect(head.scrollWidth).toBe(head.clientWidth);
  expect(label.getBoundingClientRect().height).toBeLessThan(20);
  expect(label.title).toContain(name);
});
