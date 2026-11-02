// A swarm of random user actions against a live session: menu commands,
// their keys, tab clicks, and tab drags, picked from what the menus offer.
// After each, every tab strip still shows one tab and no pane has failed;
// at the end nothing is stuck loading, and the page logged no errors. A
// failure prints its seed: `MICROSCOPE_SWARM_SEED=... just web-e2e swarm` runs it again.
// `just web-e2e` runs a short swarm; `MICROSCOPE_SWARM_STEPS=1000` runs one at depth.

import type { Page } from "@playwright/test";
import { expect, fixture, join, test } from "./server";

test.use({ program: [fixture("kvstore")] });

const STEPS = Number(process.env.MICROSCOPE_SWARM_STEPS ?? 60);

/** Commands that end what the swarm acts on, or leave the page. */
const SKIPPED = new Set(["Kill", "Restart", "Disconnect", "Copy link"]);

interface Item {
  menu: string;
  label: string;
  /** As `aria-keyshortcuts` writes it, which Playwright presses as is. */
  keys: string[];
}

/** A small seeded generator, so a failing run can be run again. */
function random(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 2 ** 32;
  };
}

/** Every item of every menu, read from the menu bar. */
async function readMenus(page: Page): Promise<Item[]> {
  const bar = page.getByRole("menubar");
  const menus = await bar
    .locator("[data-menu]")
    .evaluateAll((buttons) => buttons.map((button) => button.textContent ?? ""));
  const items: Item[] = [];
  for (const menu of menus) {
    await page.getByRole("menuitem", { name: menu, exact: true }).click();
    const listed = await page
      .getByRole("menu", { name: menu })
      .getByRole("menuitem")
      .evaluateAll((elements) =>
        elements.map((element) => ({
          label: element.getAttribute("aria-label") ?? "",
          keys: (element.getAttribute("aria-keyshortcuts") ?? "").split(" ").filter(Boolean),
        })),
      );
    items.push(...listed.map((item) => ({ menu, ...item })));
    await page.keyboard.press("Escape");
  }
  return items.filter((item) => !SKIPPED.has(item.label));
}

/** Closes whatever is open over the page: a menu, the palette, Share. A
 * floating window is part of the layout, and stays. */
async function dismiss(page: Page): Promise<void> {
  for (let tries = 0; tries < 3; tries++) {
    const open = await page.locator('[role="menu"], .overlay, .popover').count();
    if (open === 0) {
      return;
    }
    await page.keyboard.press("Escape");
  }
}

/** What must hold after every action. */
async function check(page: Page, done: string[]): Promise<void> {
  const broken = await page.evaluate(() => {
    const problems: string[] = [];
    for (const strip of document.querySelectorAll('[role="tablist"][aria-label="Panes"]')) {
      const shown = strip.querySelectorAll('[role="tab"][aria-selected="true"]').length;
      if (shown !== 1) {
        problems.push(`a tab strip shows ${shown} tabs`);
      }
    }
    for (const failed of document.querySelectorAll("[data-part=pane-failed]")) {
      problems.push(`a pane failed: ${failed.textContent}`);
    }
    return problems;
  });
  expect(broken, `after ${done.slice(-5).join(", ")}`).toEqual([]);
}

test("random commands, keys, and drags leave a working page", async ({ page, microscope }) => {
  test.setTimeout(Math.max(20_000, STEPS * 300));
  const seed = Number(process.env.MICROSCOPE_SWARM_SEED ?? Math.floor(Math.random() * 2 ** 31));
  test.info().annotations.push({ type: "seed", description: String(seed) });
  const next = random(seed);
  const pick = <T>(list: readonly T[]): T => list[Math.floor(next() * list.length)] as T;

  await join(page, microscope.link);
  const items = await readMenus(page);
  expect(items.length).toBeGreaterThan(30);
  const done: string[] = [];

  try {
    for (let step = 0; step < STEPS; step++) {
      await dismiss(page);
      const roll = next();
      if (roll < 0.4) {
        // A command from its menu, when the menu offers it now.
        const item = pick(items);
        await page.getByRole("menuitem", { name: item.menu, exact: true }).click();
        const entry = page
          .getByRole("menu", { name: item.menu })
          .getByRole("menuitem", { name: item.label, exact: true });
        // Absent now, or there and saying why it can't run.
        if ((await entry.count()) === 0 || (await entry.getAttribute("aria-disabled")) === "true") {
          await page.keyboard.press("Escape");
          done.push(`skip ${item.label}`);
        } else {
          await entry.click();
          done.push(`menu ${item.label}`);
        }
      } else if (roll < 0.7) {
        const keyed = items.filter((item) => item.keys.length > 0);
        const key = pick(pick(keyed).keys);
        await page.keyboard.press(key);
        done.push(`key ${key}`);
      } else if (roll < 0.85) {
        const tabs = page.locator('[role="tab"]:visible');
        const count = await tabs.count();
        if (count > 0) {
          const chosen = tabs.nth(Math.floor(next() * count));
          const name = await chosen.textContent();
          // A floating window may cover it, as it would for a person.
          const covered = await chosen.click({ timeout: 500 }).then(
            () => false,
            () => true,
          );
          done.push(`${covered ? "covered" : "click"} ${name}`);
        }
      } else {
        // A tab dragged to a random point of the dock.
        const tabs = page.locator('[data-part=dock] [role="tab"]:visible');
        const count = await tabs.count();
        const dock = await page.locator("[data-part=dock]").boundingBox();
        const from = count > 0 ? await tabs.nth(Math.floor(next() * count)).boundingBox() : null;
        if (from && dock) {
          const x = dock.x + next() * dock.width;
          const y = dock.y + next() * dock.height;
          done.push(`drag to ${Math.round(x)},${Math.round(y)}`);
          await page.mouse.move(from.x + from.width / 2, from.y + from.height / 2);
          await page.mouse.down();
          await page.mouse.move(x, y, { steps: 4 });
          await page.mouse.up();
        }
      }
      await check(page, done);
    }

    // Whatever was asked for arrives.
    await dismiss(page);
    await expect(page.getByText(/^Loading/).filter({ visible: true })).toHaveCount(0);
  } catch (failure) {
    throw new Error(
      `swarm seed ${seed} failed after: ${done.slice(-10).join(", ")}\n${String(failure)}`,
    );
  }
});
