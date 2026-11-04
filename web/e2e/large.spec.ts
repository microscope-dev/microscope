// Drawing large values: a million samples travel as one binary frame and
// draw as a line of at most four points a pixel column; thousands of points
// draw on a canvas.

import * as path from "node:path";

import { card, drawAt, showTab } from "./drawings";
import { expect, fixture, join, root, test } from "./server";

test("a million samples arrive at once and draw as a line that keeps their extremes", async ({
  page,
  microscope,
}) => {
  const frames: number[] = [];
  page.on("websocket", (socket) =>
    socket.on("framereceived", ({ payload }) => {
      if (typeof payload !== "string") {
        frames.push(payload.length);
      }
    }),
  );
  const server = await microscope.start([
    "--views",
    path.join(root, "tests/fixtures/c/metrics/metrics.views"),
    fixture("metrics"),
    "--",
    "--large",
  ]);
  await join(page, server.link);
  await drawAt(page, "metrics.c:119");
  const board = card(page, "m");
  const caption = board.locator(".drawing-caption");
  // The spike at 3/5 of the way survives a million samples in a few
  // hundred columns.
  await expect(caption).toContainText("n=1000000");
  await expect(caption).toContainText("max 251 at [600001]");
  // A million doubles, as bytes in one frame.
  expect(frames.some((length) => length >= 8_000_000)).toBe(true);

  const { points, width } = await board.locator(".drawing-body svg").evaluate((svg) => {
    const lines = [...svg.querySelectorAll("polyline")].map(
      (line) => (line.getAttribute("points") ?? "").trim().split(/[\s,]+/u).length / 2,
    );
    return { points: Math.max(...lines), width: (svg as SVGSVGElement).viewBox.baseVal.width };
  });
  expect(points).toBeGreaterThan(width);
  expect(points).toBeLessThanOrEqual(4 * width);

  // The Table lists every sample, a screenful at a time.
  await board.getByRole("button", { name: "Table" }).click();
  const table = board.getByRole("table", { name: "Inputs of m" });
  await expect(table).toHaveAttribute("aria-rowcount", "1000001");
  await expect(table.getByRole("row")).not.toHaveCount(0);
  expect(await table.getByRole("row").count()).toBeLessThan(40);
  await board.getByRole("button", { name: "Table" }).click();

  await showTab(board, "histogram");
  await expect(caption).toContainText("n=1000000");

  // Five thousand points are too many shapes for SVG.
  await showTab(board, "scatter-plot");
  await expect(caption).toContainText("n=5000");
  await expect(board.locator("canvas.picture")).toBeVisible();
  await expect(board.locator(".drawing-body svg")).toHaveCount(0);
});
