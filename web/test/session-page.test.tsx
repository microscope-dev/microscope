// A link to a session that is not the one running says so, without claiming
// to know it ever ran.

import { createMemoryHistory, RouterProvider } from "@tanstack/react-router";
import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { page } from "vitest/browser";
import { createAppRouter } from "../src/router";
import { SessionContext } from "../src/store";
import { fakeServer } from "./fake-server";
import { stoppedState } from "./pane";

afterEach(cleanup);

it("says a session it never saw is not here, not that it ended", async () => {
  const server = await fakeServer();
  server.send({ type: "state", ...stoppedState() });
  const router = createAppRouter(createMemoryHistory({ initialEntries: ["/s/deadbeef"] }));
  render(
    <SessionContext value={server.session}>
      <RouterProvider router={router} />
    </SessionContext>,
  );
  const notice = page.getByTestId("session-ended");
  await expect.element(notice).toBeVisible();
  expect(notice.element().textContent).toBe(
    "This session is not here. It ended, or the link is wrong. Go to basic",
  );
  await expect.element(notice.getByRole("link", { name: "Go to basic" })).toBeVisible();
});
