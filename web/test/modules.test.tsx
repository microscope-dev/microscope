// A module microscope could not read says so in words, and one with no
// separate debug file says none, as its Symbols column does.

import { afterEach, expect, it } from "vitest";
import { page } from "vitest/browser";
import { Modules } from "../src/ui/Signals";
import { mountPane, unmountPane } from "./pane";

afterEach(unmountPane);

it("names what a module's row does not know", async () => {
  const server = await mountPane(<Modules />);
  server.answer(await server.next("modules"), {
    modules: [
      {
        id: 1,
        name: "libgone.so",
        path: "/lib/libgone.so",
        start: null,
        end: null,
        symbols: "none",
        debugFile: null,
        debugFileProblem: null,
        debugInformationProblem: null,
      },
    ],
  });
  const row = page.getByRole("row").filter({ hasText: "libgone.so" });
  await expect.element(row).toBeVisible();
  const cells = [...row.element().querySelectorAll("td")].map((cell) => cell.textContent);
  expect(cells).toEqual(["libgone.so", "unknown", "unknown", "none", "/lib/libgone.so", "none"]);
});
