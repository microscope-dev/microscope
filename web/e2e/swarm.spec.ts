import type { Locator, Page } from "@playwright/test";

import { expect, fixture, join, test } from "./server";

test.use({ program: [fixture("kvstore")] });

const STEPS = Number(process.env.MICROSCOPE_SWARM_STEPS ?? 60);

const SKIPPED = new Set(["Kill", "Restart", "Disconnect", "Copy link"]);

interface Item {
  menu: string;
  label: string;

  keys: string[];
}

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
    for (const item of listed) {
      items.push({ menu, ...item });
    }
    await page.keyboard.press("Escape");
  }
  return items.filter((item) => !SKIPPED.has(item.label));
}

async function dismiss(page: Page): Promise<void> {
  for (let tries = 0; tries < 3; tries++) {
    const open = await page.locator('[role="menu"], .overlay, .popover').count();
    if (open === 0) {
      return;
    }
    await page.keyboard.press("Escape");
  }
}

async function exposed(tab: Locator): Promise<boolean> {
  await tab.scrollIntoViewIfNeeded();
  return tab.evaluate((element) => {
    const box = element.getBoundingClientRect();
    const hit = document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2);
    return hit !== null && element.contains(hit);
  });
}

async function clickTab(chosen: Locator, done: string[]): Promise<void> {
  const name = await chosen.textContent();

  if (await exposed(chosen)) {
    await chosen.click();
    await expect(chosen, `clicked ${name}`).toHaveAttribute("aria-selected", "true");
    done.push(`click ${name}`);
  } else {
    done.push(`covered ${name}`);
  }
}

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
  test.setTimeout(Math.max(20_000, STEPS * 600));
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
        const item = pick(items);
        await page.getByRole("menuitem", { name: item.menu, exact: true }).click();
        const entry = page
          .getByRole("menu", { name: item.menu })
          .getByRole("menuitem", { name: item.label, exact: true });

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
          const id = await tabs.nth(Math.floor(next() * count)).getAttribute("id");
          await clickTab(page.locator(`[id="${id}"]`), done);
        }
      } else {
        const tabs = page.locator('[data-part=dock] [role="tab"]:visible');
        const count = await tabs.count();
        const dock = await page.locator("[data-part=dock]").boundingBox();
        const chosen = count > 0 ? tabs.nth(Math.floor(next() * count)) : null;
        const from = chosen && (await exposed(chosen)) ? await chosen.boundingBox() : null;
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

    await dismiss(page);
    await expect(page.getByText(/^Loading/u).filter({ visible: true })).toHaveCount(0);
  } catch (failure) {
    throw new Error(
      `swarm seed ${seed} failed after: ${done.slice(-10).join(", ")}\n${String(failure)}`,
      { cause: failure },
    );
  }
});
