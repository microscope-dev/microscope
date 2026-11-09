// A profile's panes among the workbench's others: its overview and each of
// its views is a pane, they share what is selected through their link and
// keep how each draws it, the address follows the pane shown, and closing,
// reopening, duplicating, splitting, Back, Forward, and a reload keep it all.

import { execFileSync } from "node:child_process";
import * as path from "node:path";

import type { Locator, Page, TestInfo } from "@playwright/test";

import { expect, fixture, join, menu, root, showView, test } from "./server";

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

/** The tabs titled `title`, of `profile`'s panes when it is named. */
const tab = (page: Page, title: string, profile?: string) => {
  const titled = page.getByRole("tab").filter({
    has: page.locator("[data-part=tab-title]", { hasText: new RegExp(`^${title}$`, "u") }),
  });
  return profile === undefined ? titled : titled.filter({ hasText: profile });
};

const groupOf = (page: Page, title: string, profile?: string) =>
  page.locator("[data-group]").filter({ has: tab(page, title, profile) });

const panel = (page: Page, title: string) =>
  page.getByRole("tabpanel", { name: title, exact: true });

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

test("a profile opens as panes that share what is selected and keep how each draws it", async ({
  page,
  microscope,
}, info) => {
  const served = await microscope.start([calls(info, "leaf", "calls.usprof")]);
  await join(page, served.link);
  const profile = pathname(page);

  // The overview leads, its views open as tabs below it, and none needs
  // asking for.
  await expect(tab(page, "calls.usprof")).toHaveAttribute("aria-selected", "true");
  // Each view's tab names its profile's file.
  const views = groupOf(page, "Flame graph").getByRole("tab");
  await expect(views.nth(0)).toHaveAccessibleName("Flame graph calls.usprof");
  await expect(views.nth(1)).toHaveAccessibleName("Call tree calls.usprof");
  await expect(views.nth(2)).toHaveAccessibleName("Functions calls.usprof");
  await expect(views).toHaveCount(3);
  await expect(tab(page, "Flame graph")).toHaveAttribute("aria-selected", "true");
  await expect(tab(page, "Zones")).toHaveCount(0);
  await expect(page).not.toHaveURL(/view=/u);

  // A function chosen in one view is chosen in the others.
  await showView(page, "Functions");
  await expect(page).toHaveURL(/view=functions/u);
  await chooseFunction(page, "middle");
  await expect(page).toHaveURL(/fn=middle/u);
  await showView(page, "Call tree");
  await expect(page).toHaveURL(/view=tree/u);
  await expect(page).toHaveURL(/fn=middle/u);
  await expect(page.getByRole("searchbox", { name: "Search frames" })).toHaveValue("middle");

  // Inverting the call tree inverts only it.
  await page.getByRole("button", { name: "Options", exact: true }).click();
  await page.getByRole("dialog", { name: "Options" }).getByLabel("Inverted").check();
  await page.keyboard.press("Escape");
  await expect(panel(page, "Call tree").getByText("Callers", { exact: true })).toBeVisible();
  await expect(page).toHaveURL(/invert=1/u);
  await showView(page, "Flame graph");
  await expect(page).not.toHaveURL(/invert=/u);
  await expect(page).toHaveURL(/fn=middle/u);
  await page.getByRole("button", { name: "Options", exact: true }).click();
  await expect(
    page.getByRole("dialog", { name: "Options" }).getByLabel("Inverted"),
  ).not.toBeChecked();
  await page.keyboard.press("Escape");

  // The overview reads the same selection, and letting go of it in one
  // view lets go of it everywhere.
  await tab(page, "calls.usprof").click();
  await expect.poll(() => new URL(page.url()).search).toBe("?fn=middle");
  await showView(page, "Flame graph");
  await page.getByRole("button", { name: "Clear", exact: true }).click();
  await expect(page).not.toHaveURL(/fn=/u);
  await showView(page, "Call tree");
  await expect(page.getByRole("searchbox", { name: "Search frames" })).toHaveValue("");
  await expect(page).toHaveURL(/invert=1/u);

  // A reload keeps every pane, what it shares, and what it keeps.
  await showView(page, "Functions");
  await chooseFunction(page, "leaf");
  await page.reload();
  await expect(tab(page, "Functions")).toHaveAttribute("aria-selected", "true");
  await expect(page).toHaveURL(/view=functions/u);
  await expect(page).toHaveURL(/fn=leaf/u);
  await showView(page, "Call tree");
  await expect(page).toHaveURL(/invert=1/u);
  await expect(page).toHaveURL(/fn=leaf/u);
  await expect.poll(() => pathname(page)).toBe(profile);
});

test("a view's pane is duplicated to share its selection, or copied to keep one of its own", async ({
  page,
  microscope,
}, info) => {
  const served = await microscope.start([calls(info, "leaf", "calls.usprof")]);
  await join(page, served.link);
  await showView(page, "Functions");
  await chooseFunction(page, "middle");

  // Like this: both follow either.
  await menu(page, "Window", "New pane like this");
  await expect(tab(page, "Functions")).toHaveCount(2);
  await expect(tab(page, "Functions").nth(1)).toHaveAttribute("aria-selected", "true");
  await expect(page).toHaveURL(/fn=middle/u);
  await chooseFunction(page, "leaf");
  await tab(page, "Functions").nth(0).click();
  await expect(page).toHaveURL(/fn=leaf/u);

  // A copy, opened beside the tab it copies, starts as it was, then goes
  // its own way.
  await menu(page, "Window", "Copy to a new selection");
  await expect(tab(page, "Functions")).toHaveCount(3);
  await expect(tab(page, "Functions").nth(1)).toHaveAttribute("aria-selected", "true");
  await expect(page).toHaveURL(/fn=leaf/u);
  await chooseFunction(page, "middle");
  await expect(page).toHaveURL(/fn=middle/u);
  await tab(page, "Functions").nth(0).click();
  await expect(page).toHaveURL(/fn=leaf/u);
  await tab(page, "Call tree").click();
  await expect(page).toHaveURL(/fn=leaf/u);

  // The copy's own view is opened from the View menu, on the copy's
  // selection.
  await tab(page, "Functions").nth(1).click();
  await menu(page, "View", "Call tree");
  await expect(tab(page, "Call tree")).toHaveCount(2);
  await expect(page).toHaveURL(/view=tree/u);
  await expect(page).toHaveURL(/fn=middle/u);

  // Copies outlive what they were copied from, and a reload keeps them.
  await page.reload();
  await expect(tab(page, "Functions")).toHaveCount(3);
  await expect(tab(page, "Call tree")).toHaveCount(2);
  await expect(page).toHaveURL(/fn=middle/u);
});

test("each profile keeps its own panes and look, and the address follows the pane shown", async ({
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

  // Another profile opens its own overview and views, looking afresh. Its
  // views sit among the first's, each tab naming its profile.
  await tab(page, "Explorer").click();
  await page.getByRole("button", { name: second, exact: true }).click();
  await expect(tab(page, second)).toHaveAttribute("aria-selected", "true");
  await expect.poll(() => pathname(page)).not.toBe(firstPath);
  const secondPath = pathname(page);
  await expect(page).not.toHaveURL(/view=/u);
  await expect(tab(page, "Call tree")).toHaveCount(2);
  await expect(tab(page, "Call tree", second)).toHaveCount(1);
  await showView(page, "Functions", second);
  await chooseFunction(page, "middle");
  await expect(page).toHaveURL(/fn=middle/u);

  // Each pane, chosen, puts its profile and look in the address.
  await tab(page, "Call tree", first).click();
  await expect.poll(() => pathname(page)).toBe(firstPath);
  await expect(page).toHaveURL(/view=tree/u);
  await expect(page).not.toHaveURL(/fn=/u);
  await expect(panel(page, "Call tree").getByRole("tree", { name: "Call tree" })).toBeVisible();
  await tab(page, "Functions", second).click();
  await expect.poll(() => pathname(page)).toBe(secondPath);
  await expect(page).toHaveURL(/fn=middle/u);
  await expect(page.getByRole("tree", { name: "Callers" })).toBeVisible();

  // A reload keeps both profiles' panes and looks, and shows the one the
  // address names.
  await page.reload();
  await expect(tab(page, "Functions", second)).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("tree", { name: "Callers" })).toBeVisible();
  await tab(page, first).click();
  await expect.poll(() => pathname(page)).toBe(firstPath);
  await expect(page).not.toHaveURL(/view=/u);

  // A link to the other profile, opened in this page, shows its pane of
  // the link's view with the link's look, and opens nothing new.
  await visit(page, `${secondPath}?view=tree`);
  await expect(tab(page, "Call tree", second)).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("tree", { name: "Call tree" })).toBeVisible();
  await expect(page).not.toHaveURL(/fn=/u);
  await expect(tab(page, second)).toHaveCount(1);
  await expect(tab(page, "Call tree")).toHaveCount(2);

  // Closing a pane hands the address to the profile's pane shown after it.
  await page.keyboard.press("Alt+KeyW");
  await expect(tab(page, "Call tree", second)).toHaveCount(0);
  await expect.poll(() => new URL(page.url()).searchParams.get("view")).not.toBe("tree");
  await expect.poll(() => pathname(page)).toMatch(/^\/profile\//u);
});

test("closed panes reopen as they looked, Back and Forward step through looks, and closing the last clears the address", async ({
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
  await expect(tree.getByRole("treeitem").filter({ hasText: "middle" }).first()).toBeVisible();
  await page.goForward();
  await expect(page).toHaveURL(/tf=merge:middle/u);
  await expect(tab(page, "calls.usprof")).toHaveCount(1);
  await tab(page, "calls.usprof").click();
  await expect(page.getByText("merge middle")).toBeVisible();

  // A view closed hands the address to the pane shown in its place, and
  // comes back as it was.
  await tab(page, "Call tree").click();
  await page.keyboard.press("Alt+KeyW");
  await expect(tab(page, "Call tree")).toHaveCount(0);
  await expect(page).not.toHaveURL(/view=tree/u);
  await expect(page).toHaveURL(/tf=merge:middle/u);
  await page.keyboard.press("Alt+Shift+KeyT");
  await expect(tab(page, "Call tree")).toHaveAttribute("aria-selected", "true");
  await expect(page).toHaveURL(/view=tree/u);

  // Every pane of the profile closed, the address names none; the last
  // reopened brings back what they shared.
  for (const title of ["Call tree", "Flame graph", "Functions", "calls.usprof"]) {
    await page.getByRole("button", { name: `Close ${title}`, exact: true }).click();
  }
  await expect.poll(() => pathname(page)).toBe("/");
  await expect(page).not.toHaveURL(/tf=|view=/u);
  await page.keyboard.press("Alt+Shift+KeyT");
  await expect(tab(page, "calls.usprof")).toHaveAttribute("aria-selected", "true");
  await expect.poll(() => pathname(page)).toBe(profile);
  await expect(page).toHaveURL(/tf=merge:middle/u);
  await expect(page.getByText("merge middle")).toBeVisible();
  // Its views stay as they were left: closed.
  await expect(tab(page, "Flame graph")).toHaveCount(0);

  // Closed again, a reload leaves it closed.
  await page.keyboard.press("Alt+KeyW");
  await expect.poll(() => pathname(page)).toBe("/");
  await page.reload();
  await expect(tab(page, "Explorer")).toBeVisible();
  await expect(tab(page, "calls.usprof")).toHaveCount(0);
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

  // Each profile's Zones pane lists its own sites; the chosen one is the
  // overview's.
  await expect(tab(page, "Zones")).toHaveCount(2);
  await tab(page, "Zones", left).click();
  await expect.poll(() => pathname(page)).toBe(one);
  await expect(page).toHaveURL(/view=zones/u);
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
  await expect(page.getByTestId("accounting")).toBeVisible();
  await expect.poll(() => new URL(page.url()).search).toBe("?fn=leaf");
  await expect(page.getByRole("searchbox", { name: "Search frames" })).toHaveValue("leaf");

  // A view the profile has no pane of opens one.
  await tab(page, "Functions").click();
  await page.keyboard.press("Alt+KeyW");
  await visit(page, `${profile}?view=functions&fn=leaf`);
  await expect(tab(page, "Functions")).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("tree", { name: "Callers" })).toBeVisible();

  // A profile this microscope does not serve says how to serve it, and
  // opens no views.
  await visit(page, "/profile/0123456789abcdef?view=tree");
  const missing = page.getByTestId("profile-missing");
  await expect(missing).toContainText("No such profile");
  await expect(missing).toContainText("microscope web PROFILE.usprof");
  await expect(tab(page, "Call tree")).toHaveCount(1);
  // The served one is still a tab away.
  await tab(page, "calls.usprof").click();
  await expect.poll(() => pathname(page)).toBe(profile);
  await expect(page.getByTestId("accounting")).toContainText("Sampled6 executions");
});
