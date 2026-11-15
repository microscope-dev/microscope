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
      Number(/stop #(\d+)/u.exec(region.querySelector(".drawing-head")?.textContent ?? "")?.[1]);
    new MutationObserver(() => {
      recording.drawn.push({ stop: shown(), frame: recording.frame });
    }).observe(body, { childList: true });
    new MutationObserver(() => {
      recording.dimmed.push({ stop: shown(), stale: region.classList.contains("stale") });
    }).observe(region, { attributes: true, attributeFilter: ["class"] });
  });

  const last = 52;
  for (let stop = 3; stop <= last; stop++) {
    await page.keyboard.press("F5");
    await expect(page).toHaveURL(new RegExp(`/stop/${stop}/`, "u"));
  }

  await expect(board.locator(".drawing-head")).toContainText(`stop #${last}`);
  await expect(board.locator(".drawing-caption")).toContainText("max 301 at [361]");

  const { drawn, dimmed } = await page.evaluate(() => {
    const recording = window as unknown as Recording;
    return { drawn: recording.drawn, dimmed: recording.dimmed };
  });
  expect(drawn.length).toBeGreaterThan(0);

  expect(drawn.length).toBeLessThanOrEqual(last - 2);
  expect(drawn.at(-1)?.stop).toBe(last);
  for (let index = 1; index < drawn.length; index++) {
    const [before, after] = [drawn[index - 1] as Drawn, drawn[index] as Drawn];
    expect(after.stop, JSON.stringify(drawn)).toBeGreaterThan(before.stop);
    expect(after.frame, JSON.stringify(drawn)).toBeGreaterThan(before.frame);
  }

  expect(dimmed.some((entry) => entry.stale && entry.stop < last)).toBe(true);
  await expect(board).not.toHaveClass(/stale/u);

  await page
    .getByTestId("stops")
    .getByRole("button", { name: `#${last - 1} breakpoint · metrics.c:119` })
    .click();
  await expect(page).toHaveURL(new RegExp(`/stop/${last - 1}/`, "u"));
  await page.keyboard.press("Alt+v");
  await expect(
    page.getByText(`Drawings hidden: they belong to stop #${last - 1}, which has passed.`),
  ).toBeVisible();
});
