// Breaking, stepping, and moving through the stack, in the source the page
// shows, with the address bar naming every place.

import type { Page } from "@playwright/test";

import { expect, fixture, join, test } from "./server";

test.use({ program: [fixture("kvstore")] });

const stopUrl = /\/stop\/(\d+)\/t\/\d+\/f\/(\d+)(\?|$)/u;

/** The source line holding `text`. */
const line = (page: Page, text: string) =>
  page.getByTestId("source").locator(".cm-line", { hasText: text });

/** The gutter's number for line `n`. */
const number = (page: Page, n: number) =>
  page.locator(".cm-lineNumbers .cm-gutterElement", { hasText: new RegExp(`^${n}$`, "u") });

/**
 * Scrolls the source so line `n` is near the top: the editor only renders the
 * lines on screen, so an off-screen line has no element to click.
 */
async function scrollTo(page: Page, n: number) {
  await page
    .getByTestId("source")
    .locator(".cm-scroller")
    .evaluate((scroller, lineNumber) => {
      const height = scroller.querySelector(".cm-line")?.getBoundingClientRect().height ?? 18;
      scroller.scrollTop = Math.max(0, (lineNumber - 5) * height);
    }, n);
  await expect(number(page, n)).toBeInViewport();
}

/** Clicks the breakpoint gutter beside line `n`, which is on screen. */
async function clickGutter(page: Page, n: number) {
  const box = await number(page, n).boundingBox();
  const gutter = await page.locator(".cm-breakpoints").boundingBox();
  if (!box || !gutter) {
    throw new Error(`no gutter beside line ${n}`);
  }
  await page.mouse.click(gutter.x + gutter.width / 2, box.y + box.height / 2);
}

/** Breaks at the start of handle_request and runs to it. */
async function breakInHandleRequest(page: Page) {
  await scrollTo(page, 90);
  await line(page, "pthread_mutex_lock(&s->lock);").click();
  await page.keyboard.press("F9");
  await expect(page.getByTestId("breakpoints")).toContainText("kvstore.c:90");
  await page.keyboard.press("F5");
  await expect(page).toHaveURL(stopUrl);
  await expect(page.locator(".cm-pc-line")).toContainText("pthread_mutex_lock(&s->lock);");
  // The other worker would hit it again mid-step.
  await page.getByRole("button", { name: /Remove breakpoint/u }).click();
  await expect(page.getByTestId("breakpoints").locator("[data-breakpoint]")).toHaveCount(0);
}

async function stepOut(page: Page) {
  const before = page.url();
  await page.keyboard.press("Shift+F11");
  await expect(page).not.toHaveURL(before);
}

/** Steps over and waits for the address to name the new stop. */
async function stepOver(page: Page) {
  const before = page.url();
  await page.keyboard.press("F10");
  await expect(page).not.toHaveURL(before);
}

test("a breakpoint stops the program where the page shows, and steps replace the address", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  // Before it runs, the page opens on main (D2).
  await expect(line(page, "int main(void) {")).toBeInViewport();
  await breakInHandleRequest(page);
  await expect(page.getByRole("tabpanel", { name: "Call Stack" })).toContainText("handle_request");
  const history = await page.evaluate(() => window.history.length);

  await page.keyboard.press("F10");
  await expect(page.locator(".cm-pc-line")).toContainText("table_find(&s->table, req->key)");
  await page.keyboard.press("n");
  await expect(page.locator(".cm-pc-line")).toContainText("int status = 0;");
  // Following a stop replaces the address: Back leaves the debugger.
  expect(await page.evaluate(() => window.history.length)).toBe(history);
  await expect(page.getByTestId("stops")).toContainText("step · kvstore.c:92");

  // The caller's frame shows its line; every pane moves with it.
  await page
    .getByTestId("stack")
    .getByRole("button", { name: /worker/u })
    .click();
  await expect(page).toHaveURL(/\/f\/1(\?|$)/u);
  await expect(page.locator(".cm-frame-line")).toContainText("handle_request(&server, &req)");
  await page.keyboard.press("d");
  await expect(page).toHaveURL(/\/f\/0(\?|$)/u);
  await page.keyboard.press("u");
  await expect(page).toHaveURL(/\/f\/1(\?|$)/u);

  // Stepping out of the innermost frame returns to its caller.
  await page.keyboard.press("d");
  await expect(page).toHaveURL(/\/f\/0(\?|$)/u);
  await stepOut(page);
  await expect(page).toHaveURL(/\/f\/0(\?|$)/u);
  await expect(page.locator(".cm-pc-line")).toContainText("handle_request(&server, &req)");
});

test("Shift+J moves the thread to the cursor's line without running it", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await breakInHandleRequest(page);
  await stepOver(page);
  await stepOver(page);
  await expect(page.locator(".cm-pc-line")).toContainText("int status = 0;");
  // Back to the lookup, to run it again.
  const before = page.url();
  await line(page, "table_find(&s->table, req->key)").click();
  await page.keyboard.press("Shift+J");
  await expect(page).not.toHaveURL(before);
  await expect(page.locator(".cm-pc-line")).toContainText("table_find(&s->table, req->key)");
  await expect(page.getByTestId("stops")).toContainText("jump · kvstore.c:91");
});

test("i lists the calls of the line, and steps into the one chosen", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await breakInHandleRequest(page);
  await stepOver(page);
  await expect(page.locator(".cm-pc-line")).toContainText("table_find(&s->table, req->key)");
  const before = page.url();
  await page.keyboard.press("i");
  const calls = page.getByRole("group", { name: "Calls" });
  await expect(calls.getByRole("option")).toHaveText([/^table_find/u]);
  await page.keyboard.press("Enter");
  await expect(page).not.toHaveURL(before);
  await expect(page.getByRole("tabpanel", { name: "Call Stack" })).toContainText("table_find");
  await expect(page.getByTestId("stops")).toContainText("step · kvstore.c:66");
});

test("the gutter sets and clears breakpoints, and conditions narrow them", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await scrollTo(page, 90);
  await clickGutter(page, 90);
  const breakpoints = page.getByTestId("breakpoints");
  await expect(breakpoints).toContainText("kvstore.c:90");
  await expect(page.locator(".cm-breakpoints .dot.plain")).toHaveCount(1);

  await breakpoints.getByRole("button", { name: /Edit breakpoint/u }).click();
  await page.getByRole("textbox", { name: "Stop if" }).fill("req->op == OP_GET");
  await page.getByRole("button", { name: "Save" }).click();
  await expect(breakpoints).toContainText("if req->op == OP_GET");
  await expect(page.locator(".cm-breakpoints .dot.conditional")).toHaveCount(1);

  await page.keyboard.press("F5");
  await expect(page).toHaveURL(stopUrl);
  // Only every third request is a get, so at least two hits did not stop.
  await expect(breakpoints).toContainText(/(^|\D)([3-9]|\d\d+) hits/u);

  // The stop may have scrolled the source; measure the line where it is now.
  await scrollTo(page, 90);
  await clickGutter(page, 90);
  await expect(breakpoints.locator("[data-breakpoint]")).toHaveCount(0);
});

test("a tab that shows the latest stop follows the program, and a pinned one stays", async ({
  page,
  context,
  microscope,
}) => {
  await join(page, microscope.link);
  await breakInHandleRequest(page);
  const other = await context.newPage();
  await other.goto(page.url());
  await expect(other.locator(".cm-pc-line")).toContainText("pthread_mutex_lock");

  await stepOver(page);
  await expect(other).toHaveURL(page.url());
  await expect(other.locator(".cm-pc-line")).toContainText("table_find");
  await expect(other.getByTestId("notice")).toContainText("tester stepped over");

  // Pinned, it stays at its stop and says the program moved on.
  await other.keyboard.press("p");
  await expect(other.getByTestId("pinned")).toBeVisible();
  const pinned = other.url();
  await stepOver(page);
  await expect(other).toHaveURL(pinned);
  const passed = other.getByTestId("passed");
  await expect(passed).toContainText(/Stop #\d+ has passed/u);
  await passed.getByRole("button", { name: /Go to stop/u }).click();
  await expect(other).toHaveURL(page.url());

  // A link to a stop that has passed opens on a banner, not on newer values.
  const late = await context.newPage();
  await late.goto(pinned);
  await expect(late.getByTestId("passed")).toBeVisible();
  await expect(late.getByTestId("stack")).toHaveCount(0);
});

test("the program reads what the input line sends", async ({ page, microscope }) => {
  await join(page, microscope.link);
  await page.keyboard.press("F5");
  const input = page.getByRole("textbox", { name: "Program input" });
  await input.fill("hello there");
  await input.press("Enter");
  await expect(page.getByTestId("output")).toContainText("input: hello there");
  await input.press("Control+d");
  await expect(page.getByTestId("output")).toContainText("served");
  await expect(page.getByTestId("status")).toContainText("Exited");
});
