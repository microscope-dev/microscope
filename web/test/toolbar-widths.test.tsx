// The frame's head: the menu bar, with who is here and sharing at its end,
// and the run toolbar under it only while a program is loaded. At widths
// where it fits on one row or barely does not, the status pill stays whole
// and nothing lies over anything else.

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

async function mountFrame() {
  const server = await fakeServer();
  const router = createAppRouter(createMemoryHistory({ initialEntries: ["/"] }));
  render(
    <SessionContext value={server.session}>
      <RouterProvider router={router} />
    </SessionContext>,
  );
  return server;
}

const menuRow = () => document.querySelector(".menu-row") as HTMLElement;
const runToolbar = () => page.getByRole("toolbar", { name: "Run" });

it("shows run buttons only for a program, and the status without one only to warn", async () => {
  await page.viewport(1440, 800);
  const server = await mountFrame();
  // As a profile's session is: nothing to run.
  server.send({ type: "state", ...stoppedState({ target: null }) });
  await expect.element(page.getByRole("button", { name: "Share" })).toBeVisible();
  expect(menuRow().contains(page.getByRole("button", { name: "Share" }).element())).toBe(true);
  expect(runToolbar().elements()).toHaveLength(0);
  expect(page.getByTestId("status").elements()).toHaveLength(0);

  server.send({ type: "state", ...stoppedState({ target: null, busy: "loading /work/basic" }) });
  await expect.element(page.getByTestId("status").getByText("Working")).toBeVisible();
  expect(menuRow().contains(page.getByTestId("status").element())).toBe(true);
  expect(runToolbar().elements()).toHaveLength(0);

  server.send({ type: "state", ...stoppedState() });
  await expect.element(runToolbar()).toBeVisible();
  await expect.element(runToolbar().getByTestId("status").getByText("Stopped")).toBeVisible();
  expect(page.getByTestId("status").elements()).toHaveLength(1);
  await expect.element(runToolbar().getByRole("button", { name: /Continue/u })).toBeVisible();
});

it.each([600, 900, 1024, 1180, 1440])("keeps the head's parts apart at %ipx", async (width) => {
  await page.viewport(width, 800);
  const server = await mountFrame();
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
  await expect.element(page.getByTestId("people")).toBeVisible();

  const toolbar = document.querySelector(".toolbar") as HTMLElement;
  const status = (toolbar.querySelector(".status") as HTMLElement).getBoundingClientRect();
  const pill = (toolbar.querySelector(".status .pill") as HTMLElement).getBoundingClientRect();
  expect(pill.left).toBeGreaterThanOrEqual(status.left);
  expect(pill.right).toBeLessThanOrEqual(status.right);
  expect(toolbar.scrollWidth).toBe(toolbar.clientWidth);
  expect(menuRow().scrollWidth).toBe(menuRow().clientWidth);

  const head = document.querySelector(".frame-head") as HTMLElement;
  const parts = [
    ...head.querySelectorAll("[role=menubar] [role=menuitem], .brand, .tb, .status, .avatars"),
  ].map((element) => ({
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
