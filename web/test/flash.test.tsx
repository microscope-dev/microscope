// What the strip says a command did lasts only until the next one runs.

import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import { flash, tab } from "../src/tab";
import { useCommands } from "../src/ui/commands";
import { Strip } from "../src/ui/Strip";
import { mountPane, unmountPane } from "./pane";

afterEach(unmountPane);

function Palette() {
  const { run } = useCommands();
  return (
    <button type="button" onClick={() => run("palette")}>
      Open the palette
    </button>
  );
}

it("clears what a refused command said once another runs", async () => {
  await mountPane(
    <>
      <Palette />
      <Strip />
    </>,
  );
  tab.setState({ session: "s1" });
  flash("this is the innermost frame");
  const said = page.getByText("this is the innermost frame");
  await expect.element(said).toBeVisible();

  await userEvent.click(page.getByRole("button", { name: "Open the palette" }));
  await expect.element(said, { timeout: 1000 }).not.toBeInTheDocument();
});
