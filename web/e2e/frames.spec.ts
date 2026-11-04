// Frames of functions that left by tail calls, in the stack and in the
// values of the frame a tab selects.

import { valueRow } from "./kvstore";
import { expect, fixture, join, test } from "./server";

test.use({ program: [fixture("tail-frames-gcc-o2")] });

test("the stack marks the functions that left by tail calls", async ({ page, microscope }) => {
  await join(page, microscope.link);
  const adder = page.getByRole("textbox", { name: "Add a breakpoint" });
  await adder.fill("leaf");
  await adder.press("Enter");
  await expect(page.getByTestId("breakpoints")).toContainText("leaf");
  await adder.press("Escape");
  await page.keyboard.press("F5");
  const stack = page.getByTestId("stack");
  const frame = (name: string) =>
    stack
      .getByRole("button")
      .filter({ has: page.locator(".fn", { hasText: new RegExp(`^${name}`, "u") }) });
  await expect(frame("leaf")).not.toContainText("tail call");
  await expect(frame("middle")).toContainText("tail call");
  await expect(frame("top")).toContainText("tail call");
  await expect(frame("main")).not.toContainText("tail call");
  await frame("middle").click();
  await expect(valueRow(page, "variables", "value")).toContainText("6");
});
