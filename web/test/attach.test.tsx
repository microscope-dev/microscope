// The Attach tab's process list when listing fails and then succeeds.

import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import { SessionContext } from "../src/store";
import { AttachForm } from "../src/ui/Picker";
import { fakeServer } from "./fake-server";

afterEach(cleanup);

it("says the list failed, and a later Refresh clears the error", async () => {
  const server = await fakeServer();
  render(
    <SessionContext value={server.session}>
      <AttachForm working={false} submit={() => undefined} />
    </SessionContext>,
  );
  server.send({
    type: "error",
    id: (await server.next("processes")).id,
    error: { kind: "failed", message: "cannot read /proc" },
  });
  const list = page.getByRole("listbox", { name: "Processes" });
  await expect.element(list).toHaveTextContent("Could not list processes.");
  await expect.element(page.getByText("cannot read /proc")).toBeVisible();

  await userEvent.click(page.getByRole("button", { name: "Refresh" }));
  server.answer(await server.next("processes"), {
    processes: [{ pid: 41, command: "kvstore" }],
    ptraceScope: null,
  });
  await expect.element(page.getByRole("option", { name: /kvstore/ })).toBeVisible();
  await expect.element(page.getByText("cannot read /proc")).not.toBeInTheDocument();
});
