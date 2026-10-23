// Steps the specs that draw values share.

import type { Locator, Page } from "@playwright/test";
import { expect } from "./server";

/** Breaks at `location`, runs there, and shows the drawings. */
export async function drawAt(page: Page, location: string, stop = "#2"): Promise<void> {
  const adder = page.getByRole("textbox", { name: "Add a breakpoint" });
  await adder.fill(location);
  await adder.press("Enter");
  await expect(page.getByTestId("breakpoints")).toContainText(location);
  await adder.press("Escape");
  await page.keyboard.press("F5");
  await expect(page.getByTestId("stops")).toContainText(stop);
  await page.keyboard.press("Alt+v");
  await expect(page).toHaveURL(/[?&]view=drawings/);
}

/** The card that draws `path`. */
export const card = (page: Page, path: string) =>
  page.getByRole("region", { name: `Drawing of ${path}`, exact: true });

/** Makes Copy CSV write to `window.copied` rather than the clipboard, which
 * a headless browser may refuse. Call it before the page loads. */
export async function stubClipboard(page: Page): Promise<void> {
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        writeText: async (text: string) => {
          (window as unknown as { copied: string }).copied = text;
        },
      },
    });
  });
}

/** A card's inputs as Copy CSV copies them, by path. Keep it on screen
 * until it draws: earlier drawings can grow and push it out of view. */
export async function copyCsv(page: Page, board: Locator): Promise<Map<string, string>> {
  const button = board.getByRole("button", { name: "Copy CSV" });
  await expect
    .poll(async () => {
      await board.scrollIntoViewIfNeeded();
      return button.count();
    })
    .toBe(1);
  await page.evaluate(() => {
    (window as unknown as { copied?: string | undefined }).copied = undefined;
  });
  await button.click();
  const copied = () =>
    page.evaluate(() => (window as unknown as { copied?: string | undefined }).copied);
  await expect.poll(copied).toBeDefined();
  const text = (await copied()) as string;
  const lines = text.trimEnd().split("\n");
  expect(lines[0]).toBe("path,value");
  return new Map(lines.slice(1).map(csvRow));
}

/** One CSV row of a path and a value, either of them quoted. */
function csvRow(line: string): [string, string] {
  const fields: string[] = [];
  let at = 0;
  while (at <= line.length) {
    if (line[at] === '"') {
      let field = "";
      at += 1;
      for (;;) {
        const quote = line.indexOf('"', at);
        field += line.slice(at, quote);
        at = quote + 1;
        if (line[at] !== '"') {
          break;
        }
        field += '"';
        at += 1;
      }
      fields.push(field);
      at += 1;
    } else {
      const comma = line.indexOf(",", at);
      const end = comma < 0 || fields.length === 1 ? line.length : comma;
      fields.push(line.slice(at, end));
      at = end + 1;
    }
  }
  expect(fields, line).toHaveLength(2);
  return [fields[0] as string, fields[1] as string];
}

/** The titles of a card's shapes, in drawing order. */
export const titles = (board: Locator) =>
  board.locator(".drawing-body svg title").allTextContents();

/** Shows a card's drawing by `renderer`, among the several its value offers. */
export async function showTab(board: Locator, renderer: string): Promise<void> {
  const tab = board
    .getByRole("group", { name: /^Drawings of / })
    .getByRole("button", { name: renderer, exact: true });
  await tab.click();
  await expect(tab).toHaveAttribute("aria-pressed", "true");
}
