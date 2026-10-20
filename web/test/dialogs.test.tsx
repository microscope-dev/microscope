// A dialog takes focus when it opens and gives it back when it closes, so
// the keyboard stays where the person was.

import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import { tab } from "../src/tab";
import { KeyHelp } from "../src/ui/KeyHelp";
import { Palette } from "../src/ui/Palette";
import { mountPane, unmountPane } from "./pane";

afterEach(unmountPane);

function Before() {
  return (
    <button type="button" className="button">
      Before
    </button>
  );
}

it("moves focus into the key help and back out", async () => {
  await mountPane(
    <>
      <Before />
      <KeyHelp />
    </>,
  );
  const before = page.getByRole("button", { name: "Before" });
  await expect.element(before).toBeVisible();
  (before.element() as HTMLElement).focus();
  tab.setState({ help: true });
  const dialog = page.getByRole("dialog", { name: "Keys" });
  await expect.element(dialog).toBeVisible();
  expect(dialog.element().contains(document.activeElement)).toBe(true);
  const heads = [...dialog.element().querySelectorAll("thead th")].map((head) => head.textContent);
  expect(heads).toEqual(["Command", "Key", "Letter"]);
  tab.setState({ help: false });
  await expect.element(dialog).not.toBeInTheDocument();
  await expect.element(before).toHaveFocus();
});

it("gives focus back when the palette closes", async () => {
  await mountPane(
    <>
      <Before />
      <Palette />
    </>,
  );
  const before = page.getByRole("button", { name: "Before" });
  await expect.element(before).toBeVisible();
  (before.element() as HTMLElement).focus();
  tab.setState({ palette: "all" });
  await expect.element(page.getByRole("combobox")).toHaveFocus();
  await userEvent.keyboard("{Escape}");
  await expect.element(page.getByRole("dialog")).not.toBeInTheDocument();
  await expect.element(before).toHaveFocus();
});
