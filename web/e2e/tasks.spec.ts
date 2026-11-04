// The tasks of tokio runtimes at a stop: listed with where each waits,
// grouped by place, and chosen to show a task's own stack, which the
// address bar then names, so a link opens the same task.

import { expect, fixture, join, test } from "./server";

test.use({ program: [fixture("tokio-runtimes-o0")] });

test("tasks are listed, grouped, and chosen, and a link names the task", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  const adder = page.getByRole("textbox", { name: "Add a breakpoint" });
  await adder.fill("truth_reached");
  await adder.press("Enter");
  await expect(page.getByTestId("breakpoints")).toContainText("truth_reached");
  await adder.press("Escape");
  await page.keyboard.press("F5");
  await expect(page.getByTestId("status")).toContainText("Stopped");

  // Two runtimes and two local sets hold eight tasks, each parked.
  await page.getByRole("tab", { name: /^Tasks/u }).click();
  const tasks = page.getByTestId("tasks");
  const task = (number: number) => tasks.getByRole("button", { name: `task ${number}` });
  await expect(tasks.getByRole("button", { name: /^task \d+$/u })).toHaveCount(8);
  await expect(task(3)).toContainText("sleep");
  await expect(task(3)).toContainText("sleeping until");
  await expect(task(7)).toHaveAttribute("title", /local set/u);

  // Grouped, the four sleeping tasks share their place.
  await page.getByRole("button", { name: "group" }).click();
  const sleeping = page
    .getByTestId("task-group")
    .filter({ hasText: "4 in" })
    .filter({ hasText: "main.rs:34" });
  await expect(sleeping.getByRole("button")).toHaveText(["3", "6", "8", "9"]);
  await page.getByRole("button", { name: "ungroup" }).click();

  // Chosen, a task shows its own stack of async frames, at the frame of
  // the code the program wrote, with its locals.
  await task(7).click();
  await expect(page).toHaveURL(/\/task\/\d+\.7\/f\/1/u);
  await expect(page.getByRole("tabpanel", { name: "Variables" })).toContainText("notify");
  await expect(task(7)).toHaveAttribute("aria-current", "true");
  const stack = page.getByRole("tab", { name: /^Call Stack/u });
  await expect(stack).toContainText("task 7");
  await expect(page.getByTestId("stack")).toContainText("async");
  await expect(page.getByTestId("threads").locator("[aria-current]")).toHaveCount(0);

  // The address opens the same task.
  await page.goto(page.url());
  await expect(stack).toContainText("task 7");
  await expect(page.getByTestId("stack")).toContainText("async");
  await page.getByRole("tab", { name: /^Tasks/u }).click();
  await expect(task(7)).toHaveAttribute("aria-current", "true");
});
