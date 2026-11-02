// A failed start shows its error on its own tab, until its form is edited.

import { createMemoryHistory, RouterProvider } from "@tanstack/react-router";
import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import { createAppRouter } from "../src/router";
import { SessionContext } from "../src/store";
import { fakeServer } from "./fake-server";

afterEach(cleanup);

async function failedLaunch() {
  const server = await fakeServer();
  const router = createAppRouter(createMemoryHistory({ initialEntries: ["/pick"] }));
  render(
    <SessionContext value={server.session}>
      <RouterProvider router={router} />
    </SessionContext>,
  );
  const program = page.getByRole("combobox", { name: "Program" });
  await userEvent.fill(program, "/nope");
  await userEvent.click(page.getByRole("button", { name: "Load", exact: true }));
  server.send({
    type: "error",
    id: (await server.next("launch")).id,
    error: { kind: "failed", message: "no such program /nope" },
  });
  await expect.element(page.getByText("no such program /nope")).toBeVisible();
  return program;
}

it("hides a launch error once the program is edited", async () => {
  const program = await failedLaunch();
  await userEvent.clear(program);
  await expect.element(page.getByText("no such program /nope")).not.toBeInTheDocument();
});

it("does not show a launch error on the Attach tab", async () => {
  await failedLaunch();
  await userEvent.click(page.getByRole("button", { name: "Attach" }));
  await expect.element(page.getByText("no such program /nope")).not.toBeInTheDocument();
  await userEvent.click(page.getByRole("button", { name: "Launch" }));
  await expect.element(page.getByText("no such program /nope")).toBeVisible();
});
