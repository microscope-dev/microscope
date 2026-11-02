// Arranging the debugger's panes as a person does, in each browser: with the
// mouse onto every kind of target, with the Window keys, and across a
// reload, which keeps the arrangement.

import type { Page } from "@playwright/test";
import { expect, fixture, join, test } from "./server";

test.use({ program: [fixture("kvstore")] });

/** Each group's tabs, the shown one starred: the dock's in reading order,
 * then the windows'. */
const groups = (page: Page) =>
  page.evaluate(() =>
    [...document.querySelectorAll('[role="tablist"][aria-label="Panes"]')].map((list) =>
      [...list.querySelectorAll('[role="tab"]')].map((tab) => {
        const title = tab.querySelector(".tab-title")?.textContent ?? "";
        return tab.getAttribute("aria-selected") === "true" ? `*${title}` : title;
      }),
    ),
  );

/** The group holding a tab with this title. */
const groupOf = (page: Page, title: string) =>
  page.locator("[data-group]").filter({
    has: page.locator('[role="tab"] .tab-title', { hasText: new RegExp(`^${title}$`) }),
  });

const tab = (page: Page, title: string) =>
  page
    .getByRole("tab")
    .filter({ has: page.locator(".tab-title", { hasText: new RegExp(`^${title}$`) }) });

async function box(_page: Page, locator: ReturnType<Page["locator"]>) {
  const found = await locator.boundingBox();
  if (!found) {
    throw new Error("not on screen");
  }
  return found;
}

/** Drags from the middle of `from` to `to`, a few moves at a time. */
async function drag(
  page: Page,
  from: ReturnType<Page["locator"]>,
  to: { x: number; y: number },
  during?: () => Promise<void>,
) {
  const start = await box(page, from);
  await page.mouse.move(start.x + start.width / 2, start.y + start.height / 2);
  await page.mouse.down();
  await page.mouse.move(to.x, to.y, { steps: 6 });
  await during?.();
  await page.mouse.up();
}

test("tabs dragged onto a side, a strip, and the dock's edge land there", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await expect(tab(page, "Threads")).toBeVisible();

  // The right quarter of the Variables group splits it, Registers on the right.
  const variables = await box(page, groupOf(page, "Variables"));
  await drag(page, tab(page, "Registers"), {
    x: variables.x + variables.width * 0.85,
    y: variables.y + variables.height / 2,
  });
  // Side by side, where Variables now is: Registers' own group went, and
  // its column shared the space it left.
  await expect
    .poll(async () => (await box(page, groupOf(page, "Registers"))).x)
    .toBeCloseTo(
      (await box(page, groupOf(page, "Variables"))).x +
        (await box(page, groupOf(page, "Variables"))).width,
      -1,
    );
  expect((await box(page, groupOf(page, "Registers"))).y).toBeCloseTo(
    (await box(page, groupOf(page, "Variables"))).y,
    0,
  );

  // Between Threads and Tasks on their strip.
  const threads = await box(page, tab(page, "Threads"));
  await drag(page, tab(page, "Signals"), { x: threads.x + threads.width - 2, y: threads.y + 8 });
  await expect
    .poll(() => groups(page).then((all) => all[0]))
    .toEqual(["Threads", "*Signals", "Tasks", "Processes"]);

  // Along the bottom edge of the dock, across all of it.
  const dock = await box(page, page.locator(".dock"));
  await drag(page, tab(page, "Modules"), {
    x: dock.x + dock.width / 2,
    y: dock.y + dock.height - 4,
  });
  await expect
    .poll(async () => (await box(page, groupOf(page, "Modules"))).width)
    .toBeCloseTo(dock.width, 0);

  // Escape gives a drag up.
  const before = await groups(page);
  await drag(page, tab(page, "Watch"), { x: dock.x + 300, y: dock.y + 300 }, () =>
    page.keyboard.press("Escape"),
  );
  expect(await groups(page)).toEqual(before);
  await expect(page.locator(".drop-highlight")).toHaveCount(0);
});

test("a tab floats with Shift held, and its window moves and docks by its title bar", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await expect(tab(page, "Memory")).toBeVisible();
  const dock = await box(page, page.locator(".dock"));

  await page.keyboard.down("Shift");
  await drag(page, tab(page, "Memory"), { x: dock.x + dock.width / 2, y: dock.y + 200 });
  await page.keyboard.up("Shift");
  const window = page.getByRole("dialog", { name: "Memory" });
  await expect(window).toBeVisible();

  // Over a group's body it only moves.
  const bar = window.locator(".window-bar");
  const was = await box(page, window);
  await drag(page, bar, { x: was.x + 60, y: was.y + 100 });
  await expect.poll(async () => (await box(page, window)).y).toBeGreaterThan(was.y + 60);
  await expect(window).toBeVisible();

  // Onto a strip it docks there.
  const breakpoints = await box(page, tab(page, "Breakpoints"));
  await drag(page, bar, { x: breakpoints.x + breakpoints.width + 20, y: breakpoints.y + 8 });
  await expect(window).toHaveCount(0);
  expect((await groups(page)).find((group) => group.includes("Breakpoints"))).toEqual([
    "Breakpoints",
    "*Memory",
  ]);
});

test("the Window keys move, split, float, close, and reopen tabs, and a reload keeps it all", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await tab(page, "Output").click();

  // To the group beside, by its shown tab's panel.
  await page.keyboard.press("Alt+ArrowUp");
  await expect(page.locator("[data-group].focused")).toContainText("Disassembly");
  await page.keyboard.press("Alt+ArrowDown");
  await expect(page.locator("[data-group].focused")).toContainText("Console");

  // Along the strip, and into a group of its own.
  await page.keyboard.press("Alt+Shift+BracketRight");
  await expect
    .poll(async () => (await groups(page)).find((g) => g.includes("*Output")))
    .toEqual(["Console", "*Output", "Signals", "Modules"]);
  await page.keyboard.press("Control+Backslash");
  await expect
    .poll(async () => (await groups(page)).find((g) => g.includes("*Output")))
    .toEqual(["*Output"]);

  // Floated and docked again.
  await page.keyboard.press("Alt+Shift+KeyF");
  await expect(page.getByRole("dialog", { name: "Output" })).toBeVisible();
  await page.keyboard.press("Alt+Shift+KeyF");
  await expect(page.getByRole("dialog", { name: "Output" })).toHaveCount(0);

  // Closed and opened again.
  await page.keyboard.press("Alt+KeyW");
  await expect(tab(page, "Output")).toHaveCount(0);
  await page.keyboard.press("Alt+Shift+KeyT");
  await expect(tab(page, "Output")).toBeVisible();

  const arranged = await groups(page);
  await page.reload();
  await expect.poll(() => groups(page)).toEqual(arranged);

  // The menu bar, by keyboard.
  await page.keyboard.press("Alt+F10");
  await expect(page.getByRole("menuitem", { name: "File", exact: true })).toBeFocused();
});
