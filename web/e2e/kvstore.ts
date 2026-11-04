// Steps the specs that debug kvstore share.

import type { Page } from "@playwright/test";

import { expect } from "./server";

/** Breaks after handle_request looks its key up, and runs there. */
export async function stopInHandleRequest(page: Page): Promise<void> {
  const adder = page.getByRole("textbox", { name: "Add a breakpoint" });
  await adder.fill("kvstore.c:92");
  await adder.press("Enter");
  await expect(page.getByTestId("breakpoints")).toContainText("kvstore.c:92");
  await adder.press("Escape");
  await page.keyboard.press("F5");
  await expect(page.locator(".cm-pc-line")).toContainText("int status = 0;");
}

/** The value row named `name` in a pane. */
export const valueRow = (page: Page, pane: string, name: string) =>
  page
    .getByTestId(pane)
    .locator(".value-row")
    .filter({ has: page.locator(".value-name", { hasText: new RegExp(`^${name}$`) }) })
    .first();
