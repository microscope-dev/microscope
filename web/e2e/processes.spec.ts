// A program's children, debugged with it: the switcher beside the stop
// banner appears once a child joins, says which stopped, and switches the
// panes between processes.

import { expect, fixture, join, test } from "./server";

test.use({ program: [fixture("tree"), "--", "2", "direct", "serial"] });

test("a child that stops shows in the switcher, and the panes switch processes", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  // One process: no switcher.
  await expect(page.getByTestId("status")).toContainText("Not started");
  await expect(page.getByRole("navigation", { name: "Processes" })).toHaveCount(0);

  const add = page.getByRole("textbox", { name: "Add a breakpoint" });
  await add.fill("child_work");
  await add.press("Enter");
  await expect(page.getByTestId("breakpoints")).toContainText("child_work");
  await page.keyboard.press("F5");

  const switcher = page.getByRole("navigation", { name: "Processes" });
  const child = switcher.getByRole("button", { name: /\[2\] tree/ });
  await expect(child).toHaveAttribute("aria-pressed", "true");
  await expect(child).toContainText("forked by [1]");
  await expect(child).toContainText("stopped here");
  await expect(page.getByTestId("status")).toContainText("[2] stop #");
  await expect(page.getByTestId("stack")).toContainText("child_work");

  // The program's own process, waiting for its child.
  await switcher.getByRole("button", { name: /\[1\] tree/ }).click();
  await expect(switcher.getByRole("button", { name: /\[1\] tree/ })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
  await expect(page.getByTestId("status")).toContainText("[1] stop #");
  await expect(page.getByTestId("stack")).toContainText("main");

  // Continuing runs every process, until the next child stops.
  await page.keyboard.press("F5");
  await expect(switcher.getByRole("button", { name: /\[3\] tree/ })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
  await expect(switcher.getByRole("button", { name: /\[2\] tree/ })).toContainText("exited");
});
