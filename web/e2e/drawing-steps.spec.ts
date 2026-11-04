// Drawings while stops arrive faster than they can be drawn: a card draws
// the latest stop, at most once a frame, and never goes back to an older
// one; and a drawing of a stop that is no longer current is dimmed.

import * as path from "node:path";

import { card, drawAt } from "./drawings";
import { expect, fixture, join, root, test } from "./server";

interface Drawn {
  stop: number;
  frame: number;
}

interface Recording {
  frame: number;
  drawn: Drawn[];
  dimmed: { stop: number; stale: boolean }[];
}

test("a card keeps up with rapid stops, drawing only the latest, once a frame at most", async ({
  page,
  microscope,
}) => {
  const server = await microscope.start([
    "--views",
    path.join(root, "tests/fixtures/c/metrics/metrics.views"),
    fixture("metrics"),
  ]);
  await join(page, server.link);
  await drawAt(page, "metrics.c:119");
  const board = card(page, "m");
  await expect(board.locator(".drawing-caption")).toContainText("max 251 at [361]");

  // Each time the picture changes: the stop it shows, and the frame.
  await page.evaluate(() => {
    const recording = window as unknown as Recording;
    recording.frame = 0;
    recording.drawn = [];
    recording.dimmed = [];
    const count = () => {
      recording.frame += 1;
      requestAnimationFrame(count);
    };
    requestAnimationFrame(count);
    const region = document.querySelector('[aria-label="Drawing of m"]') as HTMLElement;
    const body = region.querySelector(".drawing-body") as HTMLElement;
    const shown = () =>
      Number(/stop #(\d+)/.exec(region.querySelector(".drawing-head")?.textContent ?? "")?.[1]);
    new MutationObserver(() => {
      recording.drawn.push({ stop: shown(), frame: recording.frame });
    }).observe(body, { childList: true });
    new MutationObserver(() => {
      recording.dimmed.push({ stop: shown(), stale: region.classList.contains("stale") });
    }).observe(region, { attributes: true, attributeFilter: ["class"] });
  });

  // Fifty stops, each continued from as soon as the page has it.
  const last = 52;
  for (let stop = 3; stop <= last; stop++) {
    await page.keyboard.press("F5");
    await expect(page).toHaveURL(new RegExp(`/stop/${stop}/`));
  }
  // Stop 52 is tick 51, whose spike is 250 + 51.
  await expect(board.locator(".drawing-head")).toContainText(`stop #${last}`);
  await expect(board.locator(".drawing-caption")).toContainText("max 301 at [361]");

  const { drawn, dimmed } = await page.evaluate(() => {
    const { drawn, dimmed } = window as unknown as Recording;
    return { drawn, dimmed };
  });
  expect(drawn.length).toBeGreaterThan(0);
  // Never more drawings than stops: the card's width stays the same.
  expect(drawn.length).toBeLessThanOrEqual(last - 2);
  expect(drawn.at(-1)?.stop).toBe(last);
  for (let index = 1; index < drawn.length; index++) {
    const [before, after] = [drawn[index - 1] as Drawn, drawn[index] as Drawn];
    expect(after.stop, JSON.stringify(drawn)).toBeGreaterThan(before.stop);
    expect(after.frame, JSON.stringify(drawn)).toBeGreaterThan(before.frame);
  }

  // While a newer stop is not yet drawn, the card is dimmed.
  expect(dimmed.some((entry) => entry.stale && entry.stop < last)).toBe(true);
  await expect(board).not.toHaveClass(/stale/);

  // A stop that has passed shows no drawings, as it shows no values.
  await page
    .getByTestId("stops")
    .getByRole("button", { name: `#${last - 1} breakpoint · metrics.c:119` })
    .click();
  await expect(page).toHaveURL(new RegExp(`/stop/${last - 1}/`));
  await page.keyboard.press("Alt+v");
  await expect(
    page.getByText(`Drawings hidden: they belong to stop #${last - 1}, which has passed.`),
  ).toBeVisible();
});
