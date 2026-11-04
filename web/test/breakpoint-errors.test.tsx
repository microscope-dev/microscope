// A refused breakpoint says why within the pane's width, however long the
// location it names.

import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";

import { Breakpoints } from "../src/ui/Breakpoints";
import { mountPane, unmountPane } from "./pane";

import "../src/styles.css";

afterEach(unmountPane);

it("wraps a long refusal instead of widening the pane", async () => {
  const server = await mountPane(
    <div style={{ width: 260 }} data-testid="column">
      <Breakpoints />
    </div>,
  );
  const location = "a".repeat(200);
  await userEvent.fill(page.getByRole("textbox", { name: "Add a breakpoint" }), location);
  await userEvent.keyboard("{Enter}");
  server.send({
    type: "error",
    id: (await server.next("addBreakpoint")).id,
    error: { kind: "invalid", message: `no function named '${location}' was found` },
  });
  await expect.element(page.getByText(/no function named/)).toBeVisible();

  const column = document.querySelector("[data-testid=column]") as HTMLElement;
  const widest = Math.max(
    // A text box scrolls within itself; nothing else may run past the pane.
    ...[...column.querySelectorAll<HTMLElement>(":not(input)")].map(
      (element) => element.scrollWidth,
    ),
  );
  expect(widest).toBeLessThanOrEqual(260);
});
