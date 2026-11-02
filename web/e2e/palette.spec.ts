// The command palette, key help, and themes.

import type { Page } from "@playwright/test";
import { stopInHandleRequest } from "./kvstore";
import { documents, expect, fixture, join, test } from "./server";

test.use({ program: [fixture("kvstore")] });

const palette = (page: Page) => page.getByRole("dialog", { name: "Command palette" });
const search = (page: Page) => palette(page).getByRole("combobox");

test("the palette finds functions, files, lines, and commands", async ({ page, microscope }) => {
  await join(page, microscope.link);
  await expect(page.locator(".cm-line").first()).toBeVisible();

  // A function opens where it is declared.
  await page.keyboard.press("Control+k");
  await search(page).fill("hreq");
  await expect(
    palette(page)
      .getByRole("option", { name: /handle_request/ })
      .first(),
  ).toBeVisible();
  await page.keyboard.press("Enter");
  await expect(palette(page)).toHaveCount(0);
  await expect(page).toHaveURL(/[?&]src=[^&]*kvstore\.c:89/);

  // Each function offers a breakpoint.
  await page.keyboard.press("Control+k");
  await search(page).fill("table_find");
  await palette(page).getByRole("option", { name: "Break at table_find" }).click();
  await expect(page.getByTestId("breakpoints")).toContainText("table_find");

  // Ctrl+P starts at files, Ctrl+G at a line of the file shown.
  await page.keyboard.press("Control+p");
  await search(page).fill("kvst");
  await expect(palette(page).getByRole("option").first()).toContainText("kvstore.c");
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/[?&]src=[^&]*kvstore\.c:1(&|$)/);
  await page.keyboard.press("Control+g");
  await search(page).fill("120");
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/[?&]src=[^&]*kvstore\.c:120(&|$)/);

  // Commands run from it, and Escape closes it without running anything.
  await page.keyboard.press("Control+k");
  await search(page).fill("theme dark");
  await page.keyboard.press("Enter");
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await expect(page.locator(".cm-line").first()).toBeVisible();
  await page.keyboard.press("Control+k");
  await search(page).fill("continue");
  await page.keyboard.press("Escape");
  await expect(palette(page)).toHaveCount(0);
  await expect(page.getByTestId("status")).toContainText("Not started");
});

test("every stop is in the palette, and ? lists every key", async ({ page, microscope }) => {
  await join(page, microscope.link);
  await stopInHandleRequest(page);
  await page.keyboard.press("F5");
  await expect(page.getByTestId("stops")).toContainText("#3");

  await page.keyboard.press("Control+k");
  await search(page).fill("#2");
  await palette(page)
    .getByRole("option", { name: /#2 breakpoint/ })
    .click();
  await expect(page).toHaveURL(/\/stop\/2\//);
  await expect(page.getByTestId("passed")).toContainText("Stop #2 has passed");

  // A stop the session never reached is not one that passed.
  await page.goto(page.url().replace(/\/stop\/2\//, "/stop/99/"));
  await expect(page.getByTestId("passed")).toContainText("This session has not reached stop #99");

  await page.keyboard.press("?");
  const keys = page.getByRole("tab", { name: "Keyboard shortcuts", exact: true });
  await expect(keys).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("row", { name: /^Step over F10/ })).toBeVisible();
});

test("closing the file shown shows its neighbor, and a middle click closes a tab", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  const files = documents(page);
  const tab = (name: string) => files.getByRole("tab", { name, exact: true });
  await expect(tab("kvstore.c")).toBeVisible();
  for (const name of ["pthread.h", "stdio.h"]) {
    await page.keyboard.press("Control+p");
    await search(page).fill(name);
    await expect(palette(page).getByRole("option").first()).toContainText(name);
    await page.keyboard.press("Enter");
    await expect(tab(name)).toHaveAttribute("aria-selected", "true");
  }

  await tab("stdio.h").click({ button: "middle" });
  await expect(tab("stdio.h")).toHaveCount(0);
  await expect(tab("pthread.h")).toHaveAttribute("aria-selected", "true");
  await expect(page).toHaveURL(/[?&]src=[^&]*pthread\.h:1(&|$)/);

  await files.getByRole("button", { name: "Close pthread.h" }).click();
  await expect(tab("pthread.h")).toHaveCount(0);
  // The program's own file follows the focus again.
  await expect(page).not.toHaveURL(/[?&]src=/);
  await expect(tab("kvstore.c")).toHaveAttribute("aria-selected", "true");

  // The last file closed leaves nothing shown.
  await files.getByRole("button", { name: "Close kvstore.c" }).click();
  await expect(tab("kvstore.c")).toHaveCount(0);
  await expect(page.locator(".cm-line")).toHaveCount(0);
  await expect(page.getByText("No file open.")).toBeVisible();
});

test("a new stop opens its file again, even on the line it was closed at", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await stopInHandleRequest(page);
  const files = documents(page);
  await files.getByRole("button", { name: "Close kvstore.c" }).click();
  await expect(page.getByText("No file open.")).toBeVisible();

  await page.keyboard.press("F5");
  await expect(page.getByTestId("stops")).toContainText("#3");
  await expect(files.getByRole("tab", { name: "kvstore.c", exact: true })).toBeVisible();
  await expect(page.locator(".cm-pc-line")).toContainText("int status = 0;");
});

test("a closed file opens again when asked for at the place it was closed", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await stopInHandleRequest(page);
  const files = documents(page);
  const closed = async () => {
    await files.getByRole("button", { name: "Close kvstore.c" }).click();
    await expect(page.getByText("No file open.")).toBeVisible();
  };
  const reopened = () =>
    expect(page.locator(".cm-line", { hasText: "int status = 0;" })).toBeVisible();

  await closed();
  await page.getByTestId("stack").getByRole("button").first().click();
  await reopened();

  await closed();
  await page
    .getByTestId("breakpoints")
    .getByRole("button", { name: /kvstore\.c:92/ })
    .click();
  await reopened();

  await closed();
  await page.keyboard.press("Control+p");
  await search(page).fill("kvstore.c");
  await page.keyboard.press("Enter");
  await expect(files.getByRole("tab", { name: "kvstore.c", exact: true })).toBeVisible();
});

test("a file's tab shows where it was, and the frame's file follows the frame", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await stopInHandleRequest(page);
  const files = documents(page);
  await page.keyboard.press("Control+g");
  await search(page).fill("stdio.h:50");
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/[?&]src=[^&]*stdio\.h:50(&|$)/);

  await files.getByRole("tab", { name: "kvstore.c", exact: true }).click();
  await expect(page).not.toHaveURL(/[?&]src=/);
  await expect(page.locator(".cm-pc-line")).toContainText("int status = 0;");

  await files.getByRole("tab", { name: "stdio.h", exact: true }).click();
  await expect(page).toHaveURL(/[?&]src=[^&]*stdio\.h:50(&|$)/);
});

test("a reload keeps the tab's files, and where each was shown", async ({ page, microscope }) => {
  await join(page, microscope.link);
  const files = documents(page);
  const tab = (name: string) => files.getByRole("tab", { name, exact: true });
  await page.keyboard.press("Control+g");
  await search(page).fill("stdio.h:50");
  await page.keyboard.press("Enter");
  await page.keyboard.press("Control+p");
  await search(page).fill("pthread.h");
  await page.keyboard.press("Enter");
  await expect(tab("pthread.h")).toHaveAttribute("aria-selected", "true");

  await page.reload();
  await expect(tab("pthread.h")).toHaveAttribute("aria-selected", "true");
  await expect(files.getByRole("button", { name: /^Close .*\.[ch]$/ })).toHaveText(["×", "×", "×"]);
  await expect(files).toContainText("kvstore.c");
  await tab("stdio.h").click();
  await expect(page).toHaveURL(/[?&]src=[^&]*stdio\.h:50(&|$)/);
});

test("closing the last file drops it from the address", async ({ page, microscope }) => {
  await join(page, microscope.link);
  await stopInHandleRequest(page);
  const files = documents(page);
  await page.keyboard.press("Control+p");
  await search(page).fill("stdio.h");
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/[?&]src=[^&]*stdio\.h/);
  await files.getByRole("button", { name: "Close kvstore.c" }).click();
  await files.getByRole("button", { name: "Close stdio.h" }).click();

  await expect(page).not.toHaveURL(/[?&]src=/);
  await expect(page.getByText("No file open.")).toBeVisible();
  await expect(files.getByRole("button", { name: /^Close .*\.[ch]$/ })).toHaveCount(0);
  // Its frame still opens its file again.
  await page.getByTestId("stack").getByRole("button").first().click();
  await expect(page.locator(".cm-pc-line")).toContainText("int status = 0;");
});
