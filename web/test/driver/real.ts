// The real backend: a Playwright page on a real `microscope web`, pressed and
// typed into as a person would, in Chromium and Firefox.

import { expect, type Page } from "@playwright/test";
import { parseChord, toPlaywright, type User, type ValuePane } from "./user";

const PANES: Record<ValuePane, string> = { Variables: "variables", Watch: "watches" };

export class RealUser implements User {
  readonly #page: Page;

  constructor(page: Page) {
    this.#page = page;
  }

  async press(chord: string): Promise<void> {
    await this.#page.keyboard.press(toPlaywright(parseChord(chord)));
  }

  async addBreakpoint(location: string): Promise<void> {
    const adder = this.#page.getByRole("textbox", { name: "Add a breakpoint" });
    await adder.fill(location);
    await adder.press("Enter");
    await expect(this.#page.getByTestId("breakpoints")).toContainText(location);
    await adder.press("Escape");
  }

  async expectCurrentLine(text: string): Promise<void> {
    await expect(this.#page.getByTestId("source").locator(".cm-pc-line")).toContainText(text);
  }

  async menu(menu: string, item: string): Promise<void> {
    await this.#page.getByRole("menuitem", { name: menu, exact: true }).click();
    await this.#page
      .getByRole("menu", { name: menu })
      .getByRole("menuitem", { name: item })
      .click();
  }

  async chooseTab(title: string): Promise<void> {
    await this.#page
      .getByRole("tab")
      .filter({ has: this.#page.getByText(title, { exact: true }) })
      .click();
  }

  async expectGroups(groups: string[][]): Promise<void> {
    await expect
      .poll(() =>
        this.#page.evaluate(() =>
          [...document.querySelectorAll('[role="tablist"][aria-label="Panes"]')].map((list) =>
            [...list.querySelectorAll('[role="tab"]')].map((tab) => {
              const title = tab.querySelector("[data-part=tab-title]")?.textContent ?? "";
              return tab.getAttribute("aria-selected") === "true" ? `*${title}` : title;
            }),
          ),
        ),
      )
      .toEqual(groups);
  }

  async expectValue(pane: ValuePane, name: string, text: string): Promise<void> {
    const row = this.#page
      .getByTestId(PANES[pane])
      .locator(".value-row")
      .filter({ has: this.#page.locator(".value-name", { hasText: new RegExp(`^${name}$`) }) })
      .first();
    await expect(row).toContainText(text);
  }
}
