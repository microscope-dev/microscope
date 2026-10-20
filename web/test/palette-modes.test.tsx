// The palette says why it lists nothing, and every item it lists does
// something or says why it cannot.

import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import { tab } from "../src/tab";
import { Palette } from "../src/ui/Palette";
import { mountPane, stoppedState, unmountPane } from "./pane";

afterEach(unmountPane);

const results = () => page.getByRole("listbox", { name: "Results" });

it("says what to type to go to a line", async () => {
  await mountPane(<Palette />);
  tab.setState({ palette: "line" });
  await expect.element(results()).toHaveTextContent("Type file:line.");
  await userEvent.keyboard("12");
  await expect.element(results()).toHaveTextContent("No file is shown; type file:line.");
});

it("says the calls are on their way, and when the stop has passed", async () => {
  await mountPane(<Palette />);
  tab.setState({ palette: "calls" });
  await expect.element(results()).toHaveTextContent("Reading the calls on this line…");
  unmountPane();
  await mountPane(<Palette />, { stale: "passed" });
  tab.setState({ palette: "calls" });
  await expect
    .element(results())
    .toHaveTextContent("Calls can be stepped into at the program's current stop.");
});

it("says why a pending breakpoint shows nothing", async () => {
  const state = stoppedState({
    breakpoints: [
      {
        id: 2,
        location: "later_fn",
        condition: null,
        hitCondition: null,
        logMessage: null,
        hits: 0,
        places: [],
      },
    ],
  });
  await mountPane(<Palette />, { state }, state);
  tab.setState({ palette: "all" });
  await userEvent.keyboard("later_fn");
  await userEvent.click(page.getByRole("option", { name: /later_fn/ }));
  expect(tab.getState().flash?.text).toBe("later_fn is pending until a module with its code loads");
});
