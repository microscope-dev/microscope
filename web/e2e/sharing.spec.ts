// Two people in one live session, through a link one of them shares.

import { expect, join, test } from "./server";

test("a view link shows a coworker the same session without its controls", async ({
  page,
  browser,
  microscope,
}) => {
  await join(page, microscope.link);
  await page.getByRole("button", { name: /Run/ }).click();
  await expect(page.getByTestId("status")).toContainText("Running");
  await page.getByRole("button", { name: /Pause/ }).click();
  await expect(page.getByTestId("status")).toContainText("Stopped");
  // The link carries the whole look, even watches holding `&` and `#`.
  await page.goto(`${page.url()}?w=${encodeURIComponent("a & b#c")}&w=d`);
  await expect(page.getByTestId("status")).toContainText("Stopped");

  await page.getByRole("button", { name: "Share" }).click();
  const dialog = page.getByRole("dialog", { name: "Share this session" });
  await expect(dialog).toBeInViewport({ ratio: 1 });
  const field = dialog.getByRole("textbox", { name: "Join link" });
  await expect(field).toHaveValue(/\/join\?to=.*#v-/);
  const link = await field.inputValue();
  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();

  const coworker = await (await browser.newContext()).newPage();
  await coworker.goto(link);
  const look = (url: string) => {
    const { pathname, searchParams } = new URL(url);
    return { pathname, watches: searchParams.getAll("w") };
  };
  await expect.poll(() => look(coworker.url())).toEqual(look(page.url()));
  expect(look(page.url()).watches).toEqual(["a & b#c", "d"]);
  await expect(coworker.getByTestId("status")).toHaveText(
    (await page.getByTestId("status").textContent()) ?? "",
  );
  await expect(coworker.getByText("View only")).toBeVisible();
  await expect(coworker.locator("[data-action]")).toHaveCount(0);
  await coworker.getByRole("menuitem", { name: "File", exact: true }).click();
  await expect(
    coworker
      .getByRole("menu", { name: "File" })
      .getByRole("menuitem", { name: "Launch a program…" }),
  ).toHaveAttribute("aria-disabled", "true");
  await coworker.keyboard.press("Escape");
  await coworker.keyboard.press("F5");
  await expect(coworker.getByTestId("status")).toContainText("Stopped");

  // Each sees the other, and the controller's continue reaches both.
  await expect(page.getByTestId("people").getByRole("listitem")).toHaveCount(2);
  await expect(coworker.getByTestId("people").getByRole("listitem")).toHaveCount(2);

  // Hovering the controller says where they look; clicking goes there.
  await page.keyboard.press("u");
  await expect(page).toHaveURL(/\/f\/1\?/);
  const owner = coworker.getByRole("button", { name: /Go to tester's focus, .*frame 1/ });
  await owner.click();
  await expect.poll(() => look(coworker.url())).toEqual(look(page.url()));

  await page.keyboard.press("F5");
  await expect(coworker.getByTestId("status")).toContainText("Running");
});
