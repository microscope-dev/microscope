import { readFileSync } from "node:fs";
import * as path from "node:path";

import { documents, expect, join, root, test } from "./server";

const script = path.join(root, "tests", "fixtures", "python", "page.py");
const python = path.join(process.env.MICROSCOPE_CPYTHON ?? "", "3.14", "python");
const marked = readFileSync(script, "utf8")
  .split("\n")
  .findIndex((line) => line.endsWith("# STOCK_LINE"));

test.use({ program: [python, "--", script] });

test("a Python breakpoint stops on the program's line, in view", async ({ page, microscope }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await join(page, microscope.link);

  const program = page.locator(".status .program");
  await expect(program).toHaveText("python3.14 page.py");
  expect(await program.evaluate((name) => name.scrollWidth <= name.clientWidth)).toBe(true);
  const location = `page.py:${marked + 1}`;
  const input = page.getByPlaceholder(/breakpoint at/u);
  await input.click();
  await input.fill(location);
  await input.press("Enter");
  await expect(page.getByTestId("breakpoints")).toContainText(location);
  await page.keyboard.press("F5");

  await expect(page).toHaveURL(/\/f\/[1-9]\d*(\?|$)/u);
  const line = page.locator(".cm-frame-line");
  await expect(line).toContainText('shelf.add(Item("bolt"');
  await expect(line).toBeInViewport();

  expect(await program.evaluate((name) => name.clientWidth >= name.scrollWidth / 2)).toBe(true);
  const variables = page.getByRole("tabpanel", { name: "Variables" });
  await expect(variables).toContainText("shelf");

  await expect(variables.locator(".value-text", { hasText: "north" })).toHaveJSProperty(
    "innerText",
    "'north    end'",
  );

  const registers = page.getByRole("tabpanel", { name: "Registers" }).locator(".empty");
  await expect(registers).toContainText("interpreted");
  await expect(registers).not.toHaveClass(/error/u);
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
    const breakpoints = page.getByRole("tabpanel", { name: "Breakpoints" });
    const input = page.getByPlaceholder(/breakpoint at/u);
    await input.click();
    await input.fill(`uat.py:${area + 1}`);
    await input.press("Enter");
    await page.keyboard.press("F5");
    await expect(page.locator(".cm-frame-line")).toContainText("result = side * side");

    await breakpoints.getByTestId("exceptions").locator("summary").click();
    const raised = breakpoints.getByRole("checkbox", { name: "Every Python exception" });
    await expect(raised).not.toBeChecked();
    const summary = breakpoints.getByTestId("exceptions").locator("summary");
    const before = Number(/(\d+) of/u.exec((await summary.textContent()) ?? "")?.[1]);
    await raised.click();
    await expect(raised).toBeChecked();
    await expect(summary).toContainText(`${before + 1} of`);
    await page.keyboard.press("F5");
    await expect(page.locator(".cm-frame-line")).toContainText(
      'raise ValueError("the shape is wrong")',
    );
    await expect(page.getByTestId("stops")).toContainText("language exception");

    await expect(page.getByRole("note")).toContainText("ValueError: the shape is wrong");

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
    await expect(source).toHaveAttribute("data-path", /\/page\.py$/u);
    await expect(source).toContainText("def stocked(");
  });
});

test.describe("a breakpoint in a script named relatively", () => {
  const uat = path.join(root, "tests", "fixtures", "python", "uat.py");
  const area = readFileSync(uat, "utf8")
    .split("\n")
    .findIndex((line) => line.endsWith("# AREA_LINE"));

  test.use({ program: [python, "--", "./tests/fixtures/python/uat.py"] });

  test("waits for its code, then is marked where it stops", async ({ page, microscope }) => {
    await join(page, microscope.link);
    const source = page.getByTestId("source");
    await expect(source).toHaveAttribute("data-path", uat);
    await source.locator(".cm-line", { hasText: "result = side * side" }).click();
    await page.keyboard.press("F9");

    const breakpoints = page.getByTestId("breakpoints");
    await expect(breakpoints).toContainText(`uat.py:${area + 1}`);
    await expect(breakpoints).not.toContainText(root);
    await expect(breakpoints).toContainText("pending until a module with its code loads");
    const waiting = page.locator(".cm-breakpoints .dot.pending");
    await expect(waiting).toHaveCount(1);
    await expect(waiting).toHaveAttribute("title", /pending until a module with its code loads/u);

    await page.keyboard.press("F5");
    await expect(page.locator(".cm-frame-line")).toContainText("result = side * side");

    const files = documents(page).getByRole("button", {
      name: /^Close (?!Disassembly$|Memory$|Drawings$)/u,
    });
    await expect(files).toHaveCount(1);
    await expect(source).toHaveAttribute("data-path", uat);
    await expect(page.locator(".cm-breakpoints .dot.plain")).toHaveCount(1);
    await expect(page.locator(".cm-breakpoints .dot.pending")).toHaveCount(0);
    await expect(breakpoints).not.toContainText("pending");
  });
});
