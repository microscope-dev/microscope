import { afterEach, expect, it } from "vitest";
import { commands } from "vitest/browser";

import { closePage, openPage } from "./driver/fast";
import { breakStepAndRead, switchPanes } from "./driver/journeys";
import { kvstore } from "./driver/scripted";

afterEach(closePage);

it("breaks, steps over a line, and reads the local it set", async () => {
  const source = await commands.readFile("../tests/fixtures/c/kvstore.c");
  const { user, debugger: scripted } = await openPage(kvstore(source));
  await breakStepAndRead(user);
  expect(scripted.unhandled.map((request) => request.method)).toEqual([]);
});

it("moves among the debug panes by tab, key, and menu", async () => {
  const source = await commands.readFile("../tests/fixtures/c/kvstore.c");
  const { user, debugger: scripted } = await openPage(kvstore(source));
  await switchPanes(user);
  expect(scripted.unhandled.map((request) => request.method)).toEqual([]);
});
