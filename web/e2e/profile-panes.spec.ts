// A profile's pane among the workbench's others: each profile tab keeps
// how it looks at its profile, the address follows the tab shown, and
// closing, reopening, splitting, Back, Forward, and a reload keep it all.

import { execFileSync } from "node:child_process";
import * as path from "node:path";

import type { Locator, Page, TestInfo } from "@playwright/test";

import { expect, fixture, join, root, showView, test } from "./server";

test.use({ program: [] });

/** Records each call of `symbol` in the stacks fixture. */
function calls(info: TestInfo, symbol: string, name: string): string {
  const file = info.outputPath(name);
  execFileSync(
    path.join(root, "target", "debug", "microscope"),
    [
      "record",
      "-o",
      file,
      "--function",
      symbol,
      "--function-period",
      "1",
      "--stack-bytes",
      "512",
      "--",
      fixture("profile-stacks-gcc-o0"),
      "1",
    ],
    { stdio: "ignore" },
  );
  return file;
}

/** Records the probes fixture's zones. */
function zones(info: TestInfo, name: string): string {
  const file = info.outputPath(name);
  execFileSync(
    path.join(root, "target", "debug", "microscope"),
    ["record", "-o", file, "--", fixture("probes"), "zones"],
    { stdio: "ignore" },
  );
  return file;
}

const tab = (page: Page, title: string) =>
  page.getByRole("tab").filter({
    has: page.locator("[data-part=tab-title]", { hasText: new RegExp(`^${title}$`, "u") }),
  });

const groupOf = (page: Page, title: string) =>
  page.locator("[data-group]").filter({ has: tab(page, title) });

/** The chip of the zone site step, in a profile's group. */
const site = (group: Locator) =>
  group.getByRole("button", { name: "Remove zone: step", exact: true });

const pathname = (page: Page) => new URL(page.url()).pathname;

/** Goes to a path of the page's own origin. */
const visit = (page: Page, to: string) => page.goto(new URL(to, page.url()).href);

/** The titles of the tabs shown in their groups. */
const shown = (page: Page) =>
  page.locator('[role="tab"][aria-selected="true"] [data-part=tab-title]').allTextContents();

async function chooseFunction(page: Page, name: string): Promise<void> {
  await page
    .getByRole("listbox", { name: "Functions" })
    .getByRole("option")
    .filter({ hasText: name })
    .click();
}

test("each profile tab keeps its own look, and the address follows the tab shown", async ({
  page,
  microscope,
}, info) => {
  const served = await microscope.start([
    calls(info, "leaf", "calls.usprof"),
    calls(info, "middle", "middle.usprof"),
  ]);
  await join(page, served.link);
  // The link opens one of them; the other opens from Explorer.
  const first = (await shown(page)).find((title) => title.endsWith(".usprof")) ?? "";
  const second = first === "calls.usprof" ? "middle.usprof" : "calls.usprof";
  const firstPath = pathname(page);
  await showView(page, "Call tree");
  await expect(page).toHaveURL(/view=tree/u);

  // Another profile opens in a tab of its own, looking at it afresh.
  await tab(page, "Explorer").click();
  await page.getByRole("button", { name: second, exact: true }).click();
  await expect(tab(page, second)).toHaveAttribute("aria-selected", "true");
  await expect.poll(() => pathname(page)).not.toBe(firstPath);
  const secondPath = pathname(page);
  await expect(page).not.toHaveURL(/view=/u);
  await showView(page, "Functions", groupOf(page, second));
  await chooseFunction(page, "middle");
  await expect(page).toHaveURL(/fn=middle/u);

  // Each tab, chosen, puts its own look in the address.
  await tab(page, first).click();
  await expect.poll(() => pathname(page)).toBe(firstPath);
  await expect(page).toHaveURL(/view=tree/u);
  await expect(page).not.toHaveURL(/fn=/u);
  await expect(groupOf(page, first).getByRole("tree", { name: "Call tree" })).toBeVisible();
  await tab(page, second).click();
  await expect.poll(() => pathname(page)).toBe(secondPath);
  await expect(page).toHaveURL(/fn=middle/u);
  await expect(groupOf(page, second).getByRole("tree", { name: "Callers" })).toBeVisible();

  // A reload keeps both tabs and both looks, and shows the one the address names.
  await page.reload();
  await expect(tab(page, second)).toHaveAttribute("aria-selected", "true");
  await expect(groupOf(page, second).getByRole("tree", { name: "Callers" })).toBeVisible();
  await tab(page, first).click();
  await expect.poll(() => pathname(page)).toBe(firstPath);
  await expect(page).toHaveURL(/view=tree/u);
  await expect(groupOf(page, first).getByRole("tree", { name: "Call tree" })).toBeVisible();

  // A link to the other profile, opened in this page, shows its tab with
  // the link's look, not the one the tab had.
  await visit(page, `${secondPath}?view=tree`);
  await expect(tab(page, second)).toHaveAttribute("aria-selected", "true");
  await expect(groupOf(page, second).getByRole("tree", { name: "Call tree" })).toBeVisible();
  await expect(page).not.toHaveURL(/fn=/u);
  await expect(tab(page, second)).toHaveCount(1);
});

test("a closed profile tab reopens as it looked, and Back and Forward step through its looks", async ({
  page,
  microscope,
}, info) => {
  const served = await microscope.start([calls(info, "leaf", "calls.usprof")]);
  await join(page, served.link);
  const profile = pathname(page);

  // Merging pushes a look; Back undoes it and Forward redoes it.
  await showView(page, "Call tree");
  const tree = page.getByRole("tree", { name: "Call tree" });
  await tree.getByRole("treeitem").filter({ hasText: "middle" }).first().click();
  await page.getByTestId("details").getByRole("button", { name: "Merge" }).click();
  await expect(page).toHaveURL(/tf=merge:middle/u);
  await expect(tree.getByRole("treeitem").filter({ hasText: "middle" })).toHaveCount(0);
  await page.goBack();
  await expect(page).not.toHaveURL(/tf=/u);
  await expect(page.getByText("merge middle")).toHaveCount(0);
  await expect(tree.getByRole("treeitem").filter({ hasText: "middle" }).first()).toBeVisible();
  await page.goForward();
  await expect(page).toHaveURL(/tf=merge:middle/u);
  await expect(page.getByText("merge middle")).toBeVisible();

  // Closed and reopened, the tab comes back as it was.
  await tab(page, "calls.usprof").click();
  await page.keyboard.press("Alt+KeyW");
  await expect(tab(page, "calls.usprof")).toHaveCount(0);
  await page.keyboard.press("Alt+Shift+KeyT");
  await expect(tab(page, "calls.usprof")).toHaveAttribute("aria-selected", "true");
  await expect.poll(() => pathname(page)).toBe(profile);
  await expect(page).toHaveURL(/tf=merge:middle/u);
  await expect(page).toHaveURL(/view=tree/u);
  await expect(page.getByText("merge middle")).toBeVisible();
});

test("two profiles side by side each keep their zones, and Escape lets go only in the one it is in", async ({
  page,
  microscope,
}, info) => {
  const served = await microscope.start([zones(info, "one.usprof"), zones(info, "two.usprof")]);
  await join(page, served.link);
  const right = (await shown(page)).find((title) => title.endsWith(".usprof")) ?? "";
  const left = right === "one.usprof" ? "two.usprof" : "one.usprof";
  const two = pathname(page);
  await visit(page, `${two}?zs=step&zf=all`);
  await expect(page.getByRole("button", { name: "Remove zone: step", exact: true })).toBeVisible();
  await tab(page, "Explorer").click();
  await page.getByRole("button", { name: left, exact: true }).click();
  await expect(tab(page, left)).toHaveAttribute("aria-selected", "true");
  const one = pathname(page);
  await visit(page, `${one}?zs=step`);

  // One profile into a group of its own, beside the other.
  await tab(page, left).click();
  await page.keyboard.press("Control+Backslash");
  await expect.poll(() => groupOf(page, left).count()).toBe(1);
  const leftGroup = groupOf(page, left);
  const rightGroup = groupOf(page, right);
  await expect(tab(page, right).and(leftGroup.getByRole("tab"))).toHaveCount(0);
  await expect(site(leftGroup)).toBeVisible();
  await expect(site(rightGroup)).toBeVisible();
  await expect(rightGroup.getByRole("button", { name: "Remove inside every one" })).toBeVisible();

  // Escape in one lets go of its site; the other keeps its own.
  await leftGroup.getByTestId("accounting").click();
  await page.keyboard.press("Escape");
  await expect(site(leftGroup)).toHaveCount(0);
  await expect(page).not.toHaveURL(/zs=/u);
  await expect(site(rightGroup)).toBeVisible();

  // Its chip, too, clears only its own.
  await rightGroup.getByTestId("accounting").click();
  await expect.poll(() => pathname(page)).toBe(two);
  await expect(page).toHaveURL(/zs=step/u);
  await site(rightGroup).click();
  await expect(site(rightGroup)).toHaveCount(0);
  await expect(page).not.toHaveURL(/zs=|zf=/u);
});

test("a link to a profile microscope does not serve says so, and a link's nonsense is dropped", async ({
  page,
  microscope,
}, info) => {
  const served = await microscope.start([calls(info, "leaf", "calls.usprof")]);
  await join(page, served.link);
  const profile = pathname(page);

  // Parts of a look that mean nothing, or nothing without another, leave
  // the address, and what they leave behind still shows.
  await visit(page, `${profile}?view=nowhere&th=abc&zb=1-2&zi=nope&range=9-1&fn=leaf`);
  await expect(page.getByTestId("profile")).toBeVisible();
  await expect.poll(() => new URL(page.url()).search).toBe("?fn=leaf");
  await expect(page.getByRole("searchbox", { name: "Search frames" })).toHaveValue("leaf");

  // A profile this microscope does not serve says how to serve it.
  await visit(page, "/profile/0123456789abcdef");
  const missing = page.getByTestId("profile-missing");
  await expect(missing).toContainText("No such profile");
  await expect(missing).toContainText("microscope web PROFILE.usprof");
  // The served one is still a tab away.
  await tab(page, "calls.usprof").click();
  await expect.poll(() => pathname(page)).toBe(profile);
  await expect(page.getByTestId("accounting")).toContainText("Sampled6 executions");
});
