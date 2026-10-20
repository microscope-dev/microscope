// Reading and changing values: the tree that stays open across stops, the
// watches a link carries, hovers and inline values, and the console.

import type { Page } from "@playwright/test";
import { stopInHandleRequest, valueRow } from "./kvstore";
import { expect, fixture, join, test } from "./server";

test.use({ program: [fixture("kvstore")] });

/** Moves the pointer over `word` in the source line holding `line`. */
async function hoverWord(page: Page, line: string, word: string) {
  const point = await page.evaluate(
    ([line, word]) => {
      const element = [...document.querySelectorAll(".cm-line")].find((candidate) =>
        candidate.textContent?.includes(line as string),
      );
      const walker = document.createTreeWalker(element as Node, NodeFilter.SHOW_TEXT);
      for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        const at = node.textContent?.indexOf(word as string) ?? -1;
        if (at >= 0) {
          const range = document.createRange();
          range.setStart(node, at + 1);
          range.setEnd(node, at + 2);
          const box = range.getBoundingClientRect();
          return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
        }
      }
      return null;
    },
    [line, word],
  );
  if (!point) {
    throw new Error(`no ${word} on the line with ${line}`);
  }
  await page.mouse.move(point.x, point.y);
}

test("the value tree stays open across stops and marks what changed", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await stopInHandleRequest(page);
  const variables = page.getByTestId("variables");
  await expect(variables).toContainText("Arguments");
  await page.getByRole("button", { name: "Open req" }).click();
  await expect(page).toHaveURL(/[?&]x=args\/req(&|$)/);
  const key = valueRow(page, "variables", "key");
  await expect(key).toContainText('"user:1000"');

  // The next hit has the next request: the same rows are open, and marked.
  await page.keyboard.press("F5");
  await expect(page.getByTestId("stops")).toContainText("#3");
  await expect(key).toContainText('"user:1001"');
  await expect(key.locator(".value-text.changed")).toContainText('was "user:1000"');
  await expect(page).toHaveURL(/[?&]x=args\/req(&|$)/);

  // Beside the lines just run, their values.
  await expect(page.locator(".cm-inline-values").first()).toBeVisible();
  await expect(page.locator(".cm-line", { hasText: "table_find(&s->table" })).toContainText(
    'req->key = "user:1001"',
  );
  await hoverWord(page, "table_find(&s->table, req->key)", "key");
  await expect(page.locator(".cm-value-tooltip")).toContainText('req->key = "user:1001"');
});

test("watches travel in the link, and a changed value reaches every tab", async ({
  page,
  context,
  microscope,
}) => {
  await join(page, microscope.link);
  await stopInHandleRequest(page);
  const add = page.getByRole("textbox", { name: "Add a watch", exact: true });
  await add.fill("s->stats.puts + 100");
  await add.press("Enter");
  await expect(page).toHaveURL(/[?&]w=s-%3Estats.puts/);
  await expect(valueRow(page, "watches", "s->stats.puts \\+ 100")).toContainText("100");

  const other = await context.newPage();
  await other.goto(page.url());
  const watched = valueRow(other, "watches", "s->stats.puts \\+ 100");
  await expect(watched).toContainText("100");

  // Double-click a value to change it; both tabs read the new one.
  await valueRow(page, "variables", "status").locator(".value-text").dblclick();
  const input = page.getByRole("textbox", { name: "New value of status" });
  await input.fill("41");
  await input.press("Enter");
  await expect(valueRow(page, "variables", "status")).toContainText("41");
  await expect(valueRow(other, "variables", "status")).toContainText("41");
  await expect(other.getByTestId("notice")).toContainText("set status = 41");
});

test("the console evaluates, runs commands, and completes in the frame shown", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await stopInHandleRequest(page);
  await page.getByRole("tab", { name: "Console" }).click();
  const line = page.getByRole("textbox", { name: "Console" });
  await line.fill("req->len * 2");
  await line.press("Enter");
  const console = page.getByTestId("console");
  await expect(console).toContainText("req->len * 2");
  await expect(console.locator(".value-row").last()).toContainText("10");

  // Commands run in the frame the tab shows.
  await page
    .getByTestId("stack")
    .getByRole("button", { name: /worker/ })
    .click();
  await line.fill("frame");
  await line.press("Enter");
  await expect(console.locator("pre").last()).toContainText("worker");

  await line.fill("ser");
  await line.press("Tab");
  await expect(line).toHaveValue("server");
  await line.fill("req->");
  await line.press("Tab");
  await expect(page.getByRole("listbox", { name: "Completions" })).toContainText("key");
  await line.press("Escape");
  await line.fill("");
  await line.press("ArrowUp");
  await expect(line).toHaveValue("frame");
});
