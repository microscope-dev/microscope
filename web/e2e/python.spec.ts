// A Python program in the page: a breakpoint set before it runs waits for
// its code, and the stop opens on the program's frame, at its line, in view.

import { readFileSync } from "node:fs";
import * as path from "node:path";
import { expect, join, root, test } from "./server";

const script = path.join(root, "tests", "fixtures", "python", "page.py");
const python = path.join(process.env.MICROSCOPE_CPYTHON ?? "", "3.14", "python");
const marked = readFileSync(script, "utf8")
  .split("\n")
  .findIndex((line) => line.endsWith("# STOCK_LINE"));

test.use({ program: [python, "--", script] });

test("a Python breakpoint stops on the program's line, in view", async ({ page, microscope }) => {
  // Tall enough that the line, below the fold, is within the editor's
  // estimate of what it shows before it measures its lines.
  await page.setViewportSize({ width: 1440, height: 900 });
  await join(page, microscope.link);
  // A laptop's header has room for the command it debugs.
  const program = page.locator(".status .program");
  await expect(program).toHaveText("python3.14 page.py");
  expect(await program.evaluate((name) => name.scrollWidth <= name.clientWidth)).toBe(true);
  const location = `page.py:${marked + 1}`;
  const input = page.getByPlaceholder(/breakpoint at/);
  await input.click();
  await input.fill(location);
  await input.press("Enter");
  await expect(page.getByTestId("breakpoints")).toContainText(location);
  await page.keyboard.press("F5");
  // The agent's frames are inside the program's, which the stop selects.
  await expect(page).toHaveURL(/\/f\/[1-9]\d*(\?|$)/);
  const line = page.locator(".cm-frame-line");
  await expect(line).toContainText('shelf.add(Item("bolt"');
  await expect(line).toBeInViewport();
  // What the stop says shares the room with the command rather than taking it.
  expect(await program.evaluate((name) => name.clientWidth >= name.scrollWidth / 2)).toBe(true);
  const variables = page.getByRole("region", { name: "Variables" });
  await expect(variables).toContainText("shelf");
  // A value is shown as it is: its spaces are not run together.
  await expect(variables.locator(".value-text", { hasText: "north" })).toHaveJSProperty(
    "innerText",
    "'north    end'",
  );
  // An interpreted frame has no registers, which is no failure.
  const registers = page.getByRole("region", { name: "Registers" }).locator(".empty");
  await expect(registers).toContainText("interpreted");
  await expect(registers).not.toHaveClass(/error/);
});

test.describe("an exception chosen as the program runs", () => {
  const uat = path.join(root, "tests", "fixtures", "python", "uat.py");
  const area = readFileSync(uat, "utf8")
    .split("\n")
    .findIndex((line) => line.endsWith("# AREA_LINE"));
  test.use({ program: [python, "--", uat] });

  test("stops where Python raises it, on the program's line", async ({ page, microscope }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await join(page, microscope.link);
    const breakpoints = page.getByRole("region", { name: "Breakpoints" });
    const input = page.getByPlaceholder(/breakpoint at/);
    await input.click();
    await input.fill(`uat.py:${area + 1}`);
    await input.press("Enter");
    await page.keyboard.press("F5");
    await expect(page.locator(".cm-frame-line")).toContainText("result = side * side");
    // Every exception stops, the interpreter's own as it starts among them,
    // so it is chosen once the program's code runs.
    await breakpoints.getByTestId("exceptions").locator("summary").click();
    const raised = breakpoints.getByRole("checkbox", { name: "Every Python exception" });
    await expect(raised).not.toBeChecked();
    const summary = breakpoints.getByTestId("exceptions").locator("summary");
    const before = Number(/(\d+) of/.exec((await summary.textContent()) ?? "")?.[1]);
    // It shows what the server chose, once it has.
    await raised.click();
    await expect(raised).toBeChecked();
    await expect(summary).toContainText(`${before + 1} of`);
    await page.keyboard.press("F5");
    await expect(page.locator(".cm-frame-line")).toContainText(
      'raise ValueError("the shape is wrong")',
    );
    await expect(page.getByTestId("stops")).toContainText("language exception");
    // The source says why the program stopped, under the line it stopped at.
    await expect(page.getByRole("note")).toContainText("ValueError: the shape is wrong");
    // However long what stopped it is, the command stays readable.
    const program = page.locator(".status .program");
    expect(await program.evaluate((name) => name.clientWidth >= name.scrollWidth / 2)).toBe(true);
  });
});

test.describe("a Python program's own files", () => {
  const uat = path.join(root, "tests", "fixtures", "python", "uat.py");
  test.use({ program: [python, "--", uat] });

  test("open on the script and from Ctrl+P before it runs", async ({ page, microscope }) => {
    await join(page, microscope.link);
    const source = page.getByTestId("source");
    await expect(source).toHaveAttribute("data-path", uat);
    await expect(source).toContainText("def area(side):");

    await page.keyboard.press("Control+p");
    await page.keyboard.type("page.py");
    const results = page.getByRole("listbox", { name: "Results" });
    await expect(results.getByRole("option").first()).toContainText("page.py");
    await page.keyboard.press("Enter");
    await expect(source).toHaveAttribute("data-path", /\/page\.py$/);
    await expect(source).toContainText("def stocked(");
  });
});

test.describe("a breakpoint in a script named relatively", () => {
  const uat = path.join(root, "tests", "fixtures", "python", "uat.py");
  const area = readFileSync(uat, "utf8")
    .split("\n")
    .findIndex((line) => line.endsWith("# AREA_LINE"));
  // As one types it, from the repository: Python records the script under
  // the working directory with its `./`.
  test.use({ program: [python, "--", "./tests/fixtures/python/uat.py"] });

  test("waits for its code, then is marked where it stops", async ({ page, microscope }) => {
    await join(page, microscope.link);
    const source = page.getByTestId("source");
    await expect(source).toHaveAttribute("data-path", uat);
    await source.locator(".cm-line", { hasText: "result = side * side" }).click();
    await page.keyboard.press("F9");

    // No code is loaded at the line yet, and the page says so.
    const breakpoints = page.getByTestId("breakpoints");
    await expect(breakpoints).toContainText(`uat.py:${area + 1}`);
    await expect(breakpoints).not.toContainText(root);
    await expect(breakpoints).toContainText("pending until a module with its code loads");
    const waiting = page.locator(".cm-breakpoints .dot.pending");
    await expect(waiting).toHaveCount(1);
    await expect(waiting).toHaveAttribute("title", /pending until a module with its code loads/);

    await page.keyboard.press("F5");
    await expect(page.locator(".cm-frame-line")).toContainText("result = side * side");
    // The stop is in the file the breakpoint was set in, which shows it set.
    await expect(page.locator(".file-tab")).toHaveCount(1);
    await expect(source).toHaveAttribute("data-path", uat);
    await expect(page.locator(".cm-breakpoints .dot.plain")).toHaveCount(1);
    await expect(page.locator(".cm-breakpoints .dot.pending")).toHaveCount(0);
    await expect(breakpoints).not.toContainText("pending");
  });
});
