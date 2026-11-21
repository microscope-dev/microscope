import { createComponent } from "solid-js";
import { expect, vi } from "vitest";
import { page, userEvent } from "vitest/browser";

import "../../src/styles.css";
import { cache } from "../../src/data";
import { forgetting } from "../../src/forget";
import { AppRouter, createMemoryHistory } from "../../src/router";
import { SessionContext } from "../../src/store";
import { initialTab, tab } from "../../src/tab";
import { resetWorkbench } from "../../src/workbench/store";
import { fakeServer, POLL } from "../fake-server";
import { cleanup, render } from "../render";
import { readGroups } from "./layout";
import { type Script, ScriptedDebugger } from "./scripted";
import { parseChord, toKeyboard, type User, type ValuePane } from "./user";

const DEADLINE = { ...POLL, timeout: 2_000 };

const PANES: Record<ValuePane, string> = { Variables: "variables", Watch: "watches" };

export async function openPage(
  script: Script,
): Promise<{ user: User; debugger: ScriptedDebugger }> {
  const server = await fakeServer();
  forgetting(server.session.store);
  const scripted = new ScriptedDebugger(server, script);
  const history = createMemoryHistory("/");

  await page.viewport(1440, 900);
  const container = document.createElement("div");
  document.body.append(container);
  container.style.height = "100vh";
  render(
    () =>
      createComponent(SessionContext, {
        value: server.session,
        get children() {
          return createComponent(AppRouter, { history });
        },
      }),
    { container },
  );
  return { user: new FastUser(), debugger: scripted };
}

export function closePage(): void {
  cleanup();
  for (const left of document.body.querySelectorAll(":scope > div:empty")) {
    left.remove();
  }
  cache.clear();
  tab.setState(initialTab, true);
  resetWorkbench();
}

function one(selector: string, within: ParentNode = document): Element {
  const found = within.querySelectorAll(selector);
  if (found.length !== 1) {
    throw new Error(`expected one ${selector}, found ${found.length}`);
  }
  return found[0] as Element;
}

class FastUser implements User {
  async press(chord: string): Promise<void> {
    await userEvent.keyboard(toKeyboard(parseChord(chord)));
  }

  async addBreakpoint(location: string): Promise<void> {
    const adder = await vi.waitFor(
      () => one('input[aria-label="Add a breakpoint"]') as HTMLInputElement,
      DEADLINE,
    );
    await userEvent.fill(adder, location);
    await userEvent.keyboard("{Enter}");
    await vi.waitFor(
      // oxlint-disable-next-line vitest/no-standalone-expect -- a driver step that tests call
      () => expect(one('[data-testid="breakpoints"]').textContent).toContain(location),
      DEADLINE,
    );
    await userEvent.keyboard("{Escape}");
  }

  async expectCurrentLine(text: string): Promise<void> {
    await vi.waitFor(
      // oxlint-disable-next-line vitest/no-standalone-expect -- a driver step that tests call
      () => expect(one('[data-testid="source"] .cm-pc-line').textContent).toContain(text),
      DEADLINE,
    );
  }

  async menu(menu: string, item: string): Promise<void> {
    await userEvent.click(page.getByRole("menuitem", { name: menu, exact: true }));
    await userEvent.click(
      page.getByRole("menu", { name: menu }).getByRole("menuitem", { name: item }),
    );
  }

  async chooseTab(title: string): Promise<void> {
    await userEvent.click(
      page.getByRole("tab").filter({ has: page.getByText(title, { exact: true }) }),
    );
  }

  async expectGroups(groups: string[][]): Promise<void> {
    // oxlint-disable-next-line vitest/no-standalone-expect -- a driver step that tests call
    await vi.waitFor(() => expect(readGroups()).toEqual(groups), DEADLINE);
  }

  async expectValue(pane: ValuePane, name: string, text: string): Promise<void> {
    await vi.waitFor(() => {
      const rows = [...one(`[data-testid="${PANES[pane]}"]`).querySelectorAll(".value-row")];
      const row = rows.find(
        (candidate) => candidate.querySelector(".value-name")?.textContent === name,
      );
      if (!row) {
        throw new Error(`${pane} has no value named ${name}`);
      }
      // oxlint-disable-next-line vitest/no-standalone-expect -- a driver step that tests call
      expect(row.querySelector(".value-text")?.firstChild?.textContent).toBe(text);
    }, DEADLINE);
  }
}
