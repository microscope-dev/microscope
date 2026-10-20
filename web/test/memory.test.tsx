// The memory view's bytes are one stop in the tab order: the arrow keys
// walk them, and Enter writes the one chosen, saying why a write failed.

import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import { Memory } from "../src/ui/Memory";
import { mountPane, unmountPane } from "./pane";

afterEach(unmountPane);

it("selects and writes bytes from the keyboard, and shows a failed write", async () => {
  const server = await mountPane(<Memory />, { look: { mem: "0x1000:2" } });
  server.answer(await server.next("readMemory"), {
    address: "0x1000",
    bytes: "0102030405060708090a0b0c0d0e0f10".repeat(16),
    unreadable: null,
  });
  const first = page.getByRole("button", { name: /at 0x1000$/ });
  await expect.element(first).toBeVisible();

  (page.getByRole("button", { name: /Later/ }).element() as HTMLElement).focus();
  await userEvent.keyboard("{Tab}");
  await expect.element(first).toHaveFocus();
  await userEvent.keyboard("{ArrowRight}");
  await expect.element(page.getByRole("button", { name: /at 0x1001$/ })).toHaveFocus();
  await userEvent.keyboard("{ArrowDown}");
  await expect.element(page.getByRole("button", { name: /at 0x1011$/ })).toHaveFocus();
  await expect.element(page.getByTestId("selection").getByText("1 byte at 0x1011")).toBeVisible();

  await userEvent.keyboard("{Enter}");
  const field = page.getByRole("textbox", { name: "New byte at 0x1011" });
  await expect.element(field).toHaveFocus();
  await userEvent.keyboard("zz{Enter}");
  await expect.element(page.getByText("pairs of hexadecimal digits")).toBeVisible();

  await userEvent.clear(field);
  await userEvent.keyboard("ff{Enter}");
  server.send({
    type: "error",
    id: (await server.next("writeMemory")).id,
    error: { kind: "failed", message: "the page is read-only" },
  });
  await expect.element(page.getByText("the page is read-only")).toBeVisible();
});

it("keeps every arrow key pressed faster than the page draws", async () => {
  const server = await mountPane(<Memory />, { look: { mem: "0x1000:2" } });
  server.answer(await server.next("readMemory"), {
    address: "0x1000",
    bytes: "00".repeat(256),
    unreadable: null,
  });
  const first = page.getByRole("button", { name: /at 0x1000$/ });
  await expect.element(first).toBeVisible();
  (first.element() as HTMLElement).focus();

  // Held keys repeat with no frame between them.
  for (const key of ["ArrowRight", "ArrowRight", "ArrowRight", "ArrowDown"]) {
    document.activeElement?.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
  }
  await expect.element(page.getByRole("button", { name: /at 0x1013$/ })).toHaveFocus();
});
