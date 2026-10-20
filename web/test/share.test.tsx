// The share popover shows the link for the access chosen last, whatever
// order the server's answers come in, and says while it is being made.

import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import { Share } from "../src/ui/Share";
import { mountPane, unmountPane } from "./pane";

afterEach(unmountPane);

it("keeps the link for the access chosen last", async () => {
  const server = await mountPane(<Share />);
  await userEvent.click(page.getByRole("button", { name: "Share" }));
  await expect.element(page.getByText("Making a link…")).toBeVisible();
  const viewing = await server.next("share");
  await userEvent.click(page.getByRole("button", { name: "Can control" }));
  const controlling = await server.next("share");

  server.answer(controlling, { url: "http://x/join?control" });
  await expect
    .element(page.getByRole("textbox", { name: "Join link" }))
    .toHaveValue("http://x/join?control");
  server.answer(viewing, { url: "http://x/join?view" });
  // The late answer would land before the next frame if it were taken.
  await new Promise((resolve) => setTimeout(resolve, 50));
  await expect
    .element(page.getByRole("textbox", { name: "Join link" }))
    .toHaveValue("http://x/join?control");
});

it("shows a failure as an error", async () => {
  const server = await mountPane(<Share />);
  await userEvent.click(page.getByRole("button", { name: "Share" }));
  server.send({
    type: "error",
    id: (await server.next("share")).id,
    error: { kind: "forbidden", message: "this link cannot share" },
  });
  await expect.element(page.getByText("this link cannot share")).toHaveClass("error");
});
