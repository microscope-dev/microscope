// The console's completions can be chosen without a mouse: the arrow keys
// walk the open list and Enter takes one. A click takes one too.

import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import { Console } from "../src/ui/Console";
import { mountPane, unmountPane } from "./pane";

afterEach(unmountPane);

const field = () => page.getByRole("textbox", { name: "Console" });

async function completeVa() {
  const server = await mountPane(<Console />);
  await userEvent.type(field(), "va");
  await userEvent.keyboard("{Tab}");
  server.answer(await server.next("complete"), {
    start: 0,
    items: [
      { label: "value", kind: "variable" },
      { label: "vars", kind: "variable" },
    ],
  });
  await expect.element(page.getByRole("listbox", { name: "Completions" })).toBeVisible();
}

it("walks the completions with the arrow keys and takes one with Enter", async () => {
  await completeVa();
  await userEvent.keyboard("{ArrowDown}{ArrowDown}");
  await expect
    .element(page.getByRole("option", { name: /vars/ }))
    .toHaveAttribute("aria-selected", "true");
  const active = field().element().getAttribute("aria-activedescendant");
  expect(active && document.getElementById(active)?.textContent).toContain("vars");
  await userEvent.keyboard("{Enter}");
  await expect.element(field()).toHaveValue("vars");
  await expect.element(page.getByRole("listbox")).not.toBeInTheDocument();
});

it("takes a clicked completion and keeps typing in the field", async () => {
  await completeVa();
  await userEvent.click(page.getByRole("option", { name: /value/ }));
  await expect.element(field()).toHaveValue("value");
  await expect.element(field()).toHaveFocus();
});
