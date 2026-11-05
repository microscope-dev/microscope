// Below the source: disassembly, memory, registers, watchpoints, signals,
// and modules.

import type { Page } from "@playwright/test";

import { stopInHandleRequest, valueRow } from "./kvstore";
import { expect, fixture, join, test } from "./server";

test.use({ program: [fixture("kvstore")] });

const disassembly = (page: Page) => page.getByTestId("disassembly");

/** The instruction the frame shown is executing. */
const marked = (page: Page) => disassembly(page).locator(".asm-row[aria-current='true']");

test("disassembly marks the frame's instruction and follows its calls", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await stopInHandleRequest(page);

  await page.keyboard.press("Alt+d");
  await expect(page).toHaveURL(/[?&]view=disassembly/u);
  await expect(disassembly(page)).toContainText("handle_request");
  await expect(marked(page)).toContainText("mov dword ptr [rbp-0xc], 0");
  const source = disassembly(page).getByRole("button", { name: /kvstore\.c:92$/u });
  await expect(source).toBeVisible();
  // The source text reads after where it is, not over it.
  const place = await source.locator(".asm-place").boundingBox();
  const text = await source.getByText("int status = 0;").boundingBox();
  expect(text && place && text.x >= place.x + place.width).toBe(true);

  // A call links where it goes, and the browser comes back.
  await disassembly(page).getByRole("link", { name: "table_find" }).click();
  await expect(page).toHaveURL(/[?&]asm=0x[0-9a-f]+/u);
  await expect(page.getByTestId("disassembly-function")).toHaveText("table_find");
  await expect(marked(page)).toHaveCount(0);
  await page.goBack();
  await expect(marked(page)).toContainText("mov dword ptr [rbp-0xc], 0");

  await page.getByRole("button", { name: "AT&T" }).click();
  await expect(marked(page)).toContainText("movl $0, -0xc(%rbp)");
  await page.getByRole("button", { name: "Intel" }).click();

  // Stepping an instruction stays in disassembly, at the next one. The
  // breakpoint goes first, so the other worker cannot stop the step.
  await page.getByRole("button", { name: "Remove breakpoint 1" }).click();
  await page.keyboard.press("Shift+S");
  await expect(marked(page)).toContainText("mov rax, [rbp-0x20]");
  await expect(page).toHaveURL(/[?&]view=disassembly/u);

  // A source line goes back to the source.
  await disassembly(page)
    .getByRole("button", { name: /kvstore\.c:93$/u })
    .click();
  await expect(page).toHaveURL(/[?&]src=[^&]*kvstore\.c:93/u);
  await expect(page).not.toHaveURL(/view=/u);

  // A frame with no source shows its code instead.
  await page
    .getByTestId("stack")
    .getByRole("button", { name: /start_thread/u })
    .click();
  await expect(page.getByTestId("no-source")).toContainText(/^start_thread\S* has no source/u);
  await expect(marked(page)).toHaveCount(1);
});

test("memory opens at a value, marks its bytes, and writes them", async ({ page, microscope }) => {
  await join(page, microscope.link);
  await stopInHandleRequest(page);
  await page.getByRole("button", { name: "Open req" }).click();
  await valueRow(page, "variables", "key")
    .getByRole("button", { name: /^Draw .*key as…$/u })
    .click();
  await page.getByRole("menuitem", { name: /^Memory/u }).click();
  await expect(page).toHaveURL(/[?&]mem=0x[0-9a-f]+:16/u);
  await expect(page).toHaveURL(/[?&]view=memory/u);

  const memory = page.getByTestId("memory");
  await expect(memory.locator(".byte.in-value")).toHaveCount(16);
  await expect(memory.locator(".byte.in-value").first()).toHaveText("75");
  await expect(page.getByTestId("selection")).toContainText('"user:1000');

  await memory.locator(".byte.in-value").first().dblclick();
  const input = page.getByRole("textbox", { name: /^New byte at 0x/u });
  await input.fill("55");
  await input.press("Enter");
  await expect(memory.locator(".byte.in-value").first()).toHaveText("55");
  await expect(valueRow(page, "variables", "key")).toContainText('"User:1000');

  await page.keyboard.press("Escape");
  await expect(page).not.toHaveURL(/view=/u);
  await expect(page.locator(".cm-pc-line")).toContainText("int status = 0;");
});

test("registers, watchpoints, signals, and modules", async ({ page, microscope }) => {
  await join(page, microscope.link);
  await stopInHandleRequest(page);

  const rip = valueRow(page, "registers", "rip");
  await expect(rip).toContainText(/0x0000[0-9a-f]{12}/u);
  await page.getByRole("button", { name: "Remove breakpoint 1" }).click();
  await page.keyboard.press("Shift+S");
  await expect(rip.locator(".value-text.changed")).toBeVisible();
  // The innermost frame's registers change in place.
  await valueRow(page, "registers", "r11").locator(".value-text").dblclick();
  const input = page.getByRole("textbox", { name: "New value of r11" });
  await input.fill("0x2a");
  await input.press("Enter");
  await expect(valueRow(page, "registers", "r11")).toContainText("0x000000000000002a");

  await valueRow(page, "variables", "status")
    .getByRole("button", { name: "Watch status for changes" })
    .click();
  const watchpoints = page.getByTestId("watchpoints");
  await expect(watchpoints).toContainText("status");
  await expect(watchpoints).toContainText("change");
  await watchpoints.getByRole("button", { name: /^Remove watchpoint/u }).click();
  await expect(watchpoints).toHaveCount(0);

  await page.getByRole("tab", { name: "Signals" }).click();
  const stop = page.getByRole("checkbox", { name: "Stop on SIGUSR1" });
  await expect(stop).toBeChecked();
  // The box shows the policy the debugger has, once it has it.
  await stop.click();
  await expect(stop).not.toBeChecked();

  await page.getByRole("tab", { name: "Modules" }).click();
  const modules = page.getByTestId("modules");
  await expect(modules).toContainText("kvstore");
  await expect(modules).toContainText("libc");
});
