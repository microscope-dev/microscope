// The whole page's workbench at a stop: the Debug layout fits common window
// sizes, a pane in a tab not shown asks the server nothing, the menu bar
// works by keyboard and says why a command cannot run, and the browser keeps
// the layout across a reload.

import { afterEach, expect, it } from "vitest";
import { commands, page, userEvent } from "vitest/browser";
import { closePage, openPage } from "./driver/fast";
import { readGroups } from "./driver/layout";
import { kvstore } from "./driver/scripted";

afterEach(closePage);

async function stopped() {
  const source = await commands.readFile("../tests/fixtures/c/kvstore.c");
  const opened = await openPage(kvstore(source));
  await opened.user.addBreakpoint("kvstore.c:92");
  await opened.user.press("F5");
  await opened.user.expectCurrentLine("int status = 0;");
  return opened;
}

it.each([
  [1440, 900],
  [1280, 720],
  [1024, 640],
])("lays the Debug layout out at %ix%i, every group whole and apart", async (width, height) => {
  await stopped();
  await page.viewport(width, height);
  expect(readGroups()).toHaveLength(8);

  const boxes = [...document.querySelectorAll("[data-group]")].map((group) =>
    group.getBoundingClientRect(),
  );
  const dock = (document.querySelector(".dock") as HTMLElement).getBoundingClientRect();
  for (const [index, box] of boxes.entries()) {
    // A tab strip and a few rows at least.
    expect(box.width, `group ${index}`).toBeGreaterThanOrEqual(150);
    expect(box.height, `group ${index}`).toBeGreaterThanOrEqual(80);
    expect(box.right).toBeLessThanOrEqual(dock.right + 0.5);
    expect(box.bottom).toBeLessThanOrEqual(dock.bottom + 0.5);
    for (const other of boxes.slice(index + 1)) {
      const apart =
        box.right <= other.left + 0.5 ||
        other.right <= box.left + 0.5 ||
        box.bottom <= other.top + 0.5 ||
        other.bottom <= box.top + 0.5;
      expect(apart, `group ${index} overlaps another`).toBe(true);
    }
  }
  // Each strip stays one line, whatever its tabs say.
  for (const strip of document.querySelectorAll('[role="tablist"][aria-label="Panes"]')) {
    expect(strip.getBoundingClientRect().height).toBe(28);
  }
});

it("asks nothing for a pane whose tab is not shown, until it is", async () => {
  const { user, debugger: scripted } = await stopped();
  expect(scripted.asked("signals")).toBe(0);
  expect(scripted.asked("disassemble")).toBe(0);
  await user.chooseTab("Signals");
  await expect.poll(() => scripted.asked("signals")).toBe(1);
  expect(scripted.asked("disassemble")).toBe(0);
});

it("opens a menu by keyboard, walks it, and says why an item cannot run", async () => {
  await stopped();
  const run = page.getByRole("menuitem", { name: "Run", exact: true });
  (run.element() as HTMLElement).focus();
  await userEvent.keyboard("{ArrowDown}");
  const menu = page.getByRole("menu", { name: "Run" });
  await expect.element(menu).toBeVisible();
  await expect.element(menu.getByRole("menuitem", { name: "Continue" })).toHaveFocus();
  // The program is stopped, so Pause cannot run, and says so.
  const pause = menu.getByRole("menuitem", { name: "Pause" });
  await expect.element(pause).toHaveAttribute("aria-disabled", "true");
  expect(pause.element().getAttribute("title")).toMatch(/running|stopped/);
  await expect.element(pause).toHaveAttribute("aria-keyshortcuts", "F6");

  await userEvent.keyboard("{ArrowRight}");
  await expect.element(page.getByRole("menu", { name: "Debug" })).toBeVisible();
  await userEvent.keyboard("{Escape}");
  await expect.element(page.getByRole("menu")).not.toBeInTheDocument();
  await expect.element(page.getByRole("menuitem", { name: "Debug", exact: true })).toHaveFocus();
});

it("keeps this browser's layout across a reload, and says once when it can't", async () => {
  const source = await commands.readFile("../tests/fixtures/c/kvstore.c");
  const first = await openPage(kvstore(source));
  await first.user.chooseTab("Registers");
  await first.user.press("Alt+Enter");
  // Maximized, the Registers group fills the dock alone.
  await first.user.expectGroups([["*Registers"]]);
  closePage();

  const second = await openPage(kvstore(source));
  await second.user.expectGroups([["*Registers"]]);
  closePage();

  localStorage.setItem("microscope-layout", '{"version":1,"layout":{"root":"split"}}');
  await openPage(kvstore(source));
  await expect.poll(() => readGroups()).toHaveLength(8);
  await expect
    .poll(() => document.querySelector("[data-testid=flash]")?.textContent)
    .toMatch(/kept can't be used \(panes is not an object\), so it is the Debug layout/);
});

it("shows one group at a time in a narrow window, and any pane from its list", async () => {
  const source = await commands.readFile("../tests/fixtures/c/kvstore.c");
  await openPage(kvstore(source));
  await page.viewport(600, 700);
  await expect.poll(() => readGroups()).toEqual([["*Threads", "Tasks", "Processes"]]);
  await userEvent.selectOptions(page.getByRole("combobox", { name: "Pane" }), "Registers");
  await expect.poll(() => readGroups()).toEqual([["*Registers"]]);
  await expect.element(page.getByRole("tabpanel", { name: "Registers" })).toBeVisible();
});
