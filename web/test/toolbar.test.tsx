// The run buttons send what was clicked once, however fast the clicks come.

import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import { tab } from "../src/tab";
import { Toolbar } from "../src/ui/Toolbar";
import { POLL } from "./fake-server";
import { mountPane, unmountPane } from "./pane";

afterEach(unmountPane);

it("continues once for a double click", async () => {
  const server = await mountPane(<Toolbar />);
  tab.setState({ at: { stop: 1, thread: 1, frame: 0 } });
  const sent: string[] = [];
  server.onRequest((request) => sent.push(request.method));
  const button = page.getByRole("button", { name: /Continue/ });
  await expect.element(button).toBeEnabled();

  await userEvent.dblClick(button);
  await expect.poll(() => sent, POLL).toContain("continue");
  // The second click lands while the first is still being answered.
  await new Promise((resolve) => setTimeout(resolve, 20));
  expect(sent.filter((method) => method === "continue")).toEqual(["continue"]);
  server.answer(await server.next("continue"), null);
});
