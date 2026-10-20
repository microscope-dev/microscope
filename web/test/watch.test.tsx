// A watch the tab cannot read now says so once, above the list, and its
// row is dim; only an expression that failed to evaluate is red.

import { afterEach, expect, it } from "vitest";
import { page } from "vitest/browser";
import { Registers } from "../src/ui/Registers";
import { Watch } from "../src/ui/Values";
import { mountPane, unmountPane } from "./pane";

afterEach(unmountPane);

const textOf = (name: string) =>
  page.getByRole("listitem").filter({ hasText: name }).element().querySelector(".value-text");

it("dims a watch hidden while the program runs, saying why once", async () => {
  await mountPane(<Watch />, { look: { w: ["count", "limit"] }, stale: "running" });
  await expect
    .element(page.getByText("The program is running; values appear when it stops."))
    .toBeVisible();
  await expect
    .element(page.getByRole("listitem").filter({ hasText: "count" }).getByText("not read"))
    .toBeVisible();
  expect(textOf("count")?.className).toContain("absent");
  expect(textOf("count")?.className).not.toContain("error");
});

it("marks a watch red only when it failed", async () => {
  const server = await mountPane(<Watch />, { look: { w: ["nope"] } });
  server.send({
    type: "error",
    id: (await server.next("evaluate")).id,
    error: { kind: "failed", message: "no variable nope" },
  });
  await expect.element(page.getByText("no variable nope")).toBeVisible();
  expect(textOf("nope")?.className).toContain("error");
});

it("dims an outer frame's register that was not saved", async () => {
  const server = await mountPane(<Registers />, { at: { stop: 1, thread: 1, frame: 1 } });
  server.answer(await server.next("registers"), {
    registers: [{ name: "rbx", value: null, bits: 64, role: null }],
  });
  await expect.element(page.getByText("not saved")).toBeVisible();
  expect(textOf("rbx")?.className).toContain("absent");
  expect(textOf("rbx")?.className).not.toContain("error");
});
