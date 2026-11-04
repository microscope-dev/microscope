// The toolbar at widths where it fits on one row or barely does not: the
// status pill stays whole, and nothing lies over anything else.

import { createMemoryHistory, RouterProvider } from "@tanstack/react-router";
import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { page } from "vitest/browser";

import "../src/styles.css";
import { createAppRouter } from "../src/router";
import { SessionContext } from "../src/store";
import { fakeServer } from "./fake-server";
import { stoppedState } from "./pane";

afterEach(cleanup);

it.each([900, 1024, 1180, 1440])("keeps the toolbar's parts apart at %ipx", async (width) => {
  await page.viewport(width, 800);
  const server = await fakeServer();
  const program = `/work/${"long-program-name-".repeat(6)}basic`;
  server.send({
    type: "state",
    ...stoppedState({ target: { kind: "launch", program, arguments: [], pid: 100 } }),
  });
  server.send({
    type: "presence",
    people: [
      { connection: 1, name: "tester", role: "control", focus: null },
      { connection: 2, name: "pat", role: "view", focus: null },
    ],
  });
  const router = createAppRouter(createMemoryHistory({ initialEntries: ["/"] }));
  render(
    <SessionContext value={server.session}>
      <RouterProvider router={router} />
    </SessionContext>,
  );
  await expect.element(page.getByTestId("people")).toBeVisible();

  const toolbar = document.querySelector(".toolbar") as HTMLElement;
  const status = (toolbar.querySelector(".status") as HTMLElement).getBoundingClientRect();
  const pill = (toolbar.querySelector(".status .pill") as HTMLElement).getBoundingClientRect();
  expect(pill.left).toBeGreaterThanOrEqual(status.left);
  expect(pill.right).toBeLessThanOrEqual(status.right);
  expect(toolbar.scrollWidth).toBe(toolbar.clientWidth);

  const parts = [...toolbar.querySelectorAll(".brand, .tb, .status, .avatars")].map((element) => ({
    name: element.textContent,
    box: element.getBoundingClientRect(),
  }));
  for (const [index, a] of parts.entries()) {
    for (const b of parts.slice(index + 1)) {
      const apart =
        a.box.right <= b.box.left ||
        b.box.right <= a.box.left ||
        a.box.bottom <= b.box.top ||
        b.box.bottom <= a.box.top;
      expect(apart, `${a.name} and ${b.name}`).toBe(true);
    }
  }
});
