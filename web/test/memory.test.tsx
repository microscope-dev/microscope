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
  const first = page.getByRole("button", { name: /at 0x1000$/u });
  await expect.element(first).toBeVisible();

  (page.getByRole("button", { name: /Later/u }).element() as HTMLElement).focus();
  await userEvent.keyboard("{Tab}");
  await expect.element(first).toHaveFocus();
  await userEvent.keyboard("{ArrowRight}");
  await expect.element(page.getByRole("button", { name: /at 0x1001$/u })).toHaveFocus();
  await userEvent.keyboard("{ArrowDown}");
  await expect.element(page.getByRole("button", { name: /at 0x1011$/u })).toHaveFocus();
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
  const first = page.getByRole("button", { name: /at 0x1000$/u });
  await expect.element(first).toBeVisible();
  (first.element() as HTMLElement).focus();

  // Held keys repeat with no frame between them.
  for (const key of ["ArrowRight", "ArrowRight", "ArrowRight", "ArrowDown"]) {
    document.activeElement?.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
  }
  await expect.element(page.getByRole("button", { name: /at 0x1013$/u })).toHaveFocus();
});

it("shows the last page of the address space without running past its end", async () => {
  const server = await mountPane(<Memory />, { look: { mem: "0xffffffffffffffff:1" } });
  const asked = await server.next("readMemory");
  expect(asked.params).toMatchObject({ address: "0xffffffffffffff00", count: 256 });
  server.answer(asked, {
    address: "0xffffffffffffff00",
    bytes: "",
    unreadable: "0xffffffffffffff00",
  });
  await expect.element(page.getByText("fffffffffffffff0", { exact: true })).toBeVisible();
  expect(document.body.textContent).not.toMatch(/10000000000000000/u);
  await expect.element(page.getByRole("button", { name: /Later/u })).toBeDisabled();
});

/** The byte at `offset` past 0x1000. */
const byte = (offset: number) =>
  page.getByRole("button", { name: new RegExp(`at 0x${(0x1000 + offset).toString(16)}$`, "u") });
const classes = (offset: number) => (byte(offset).element() as HTMLElement).className.split(/\s+/u);

it("selects a character array's string and marks the rest of the array", async () => {
  const server = await mountPane(<Memory />, { look: { mem: "0x1000:16:7" } });
  // "hello\n", its NUL, and what the array held before.
  server.answer(await server.next("readMemory"), {
    address: "0x1000",
    bytes: `68656c6c6f0a00${"ab".repeat(9)}${"00".repeat(240)}`,
    unreadable: null,
  });
  await expect.element(byte(0)).toBeVisible();

  for (const offset of [0, 6]) {
    expect(classes(offset)).toEqual(expect.arrayContaining(["in-value", "selected"]));
  }
  for (const offset of [7, 15]) {
    expect(classes(offset)).toContain("in-value");
    expect(classes(offset)).not.toContain("selected");
  }
  expect(classes(16)).not.toContain("in-value");
  await expect.element(page.getByTestId("selection").getByText("7 bytes at 0x1000")).toBeVisible();
  await expect.element(page.getByText(/16 bytes · string: 7 bytes/u)).toBeVisible();
});
