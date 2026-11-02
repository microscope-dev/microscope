// The fast backend: the whole page, rendered in Vitest's Chromium against a
// scripted debugger, with keys pressed through the browser as a person
// presses them. A journey runs here in a few hundred milliseconds.

import { createMemoryHistory, RouterProvider } from "@tanstack/react-router";
import { cleanup, render } from "@testing-library/react";
import { createElement } from "react";
import { expect, vi } from "vitest";
import { userEvent } from "vitest/browser";
import "../../src/styles.css";
import { cache } from "../../src/data";
import { forgetting } from "../../src/forget";
import { createAppRouter } from "../../src/router";
import { SessionContext } from "../../src/store";
import { initialTab, tab } from "../../src/tab";
import { fakeServer, POLL } from "../fake-server";
import { type Script, ScriptedDebugger } from "./scripted";
import { parseChord, toKeyboard, type User, type ValuePane } from "./user";

/** Waits this long for what a journey expects before failing. */
const DEADLINE = { ...POLL, timeout: 2_000 };

const PANES: Record<ValuePane, string> = { Variables: "variables", Watch: "watches" };

/** Renders the page at `/`, served by a debugger following `script`. */
export async function openPage(
  script: Script,
): Promise<{ user: User; debugger: ScriptedDebugger }> {
  const server = await fakeServer();
  forgetting(server.session.store);
  const scripted = new ScriptedDebugger(server, script);
  const router = createAppRouter(createMemoryHistory({ initialEntries: ["/"] }));
  render(
    createElement(
      SessionContext,
      { value: server.session },
      createElement(RouterProvider, { router }),
    ),
  );
  return { user: new FastUser(), debugger: scripted };
}

/** Forgets what one journey left: its page, its answers, and its tab. */
export function closePage(): void {
  cleanup();
  cache.clear();
  tab.setState(initialTab, true);
}

/** The single element `selector` finds, or why there is not one. */
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
      () => expect(one('[data-testid="breakpoints"]').textContent).toContain(location),
      DEADLINE,
    );
    await userEvent.keyboard("{Escape}");
  }

  async expectCurrentLine(text: string): Promise<void> {
    await vi.waitFor(
      () => expect(one('[data-testid="source"] .cm-pc-line').textContent).toContain(text),
      DEADLINE,
    );
  }

  async expectValue(pane: ValuePane, name: string, text: string): Promise<void> {
    await vi.waitFor(() => {
      const rows = [...one(`[data-testid="${PANES[pane]}"]`).querySelectorAll(".value-row")];
      const row = rows.find(
        (candidate) => candidate.querySelector(".value-name")?.textContent === name,
      );
      expect(row, `${pane} has no value named ${name}`).toBeDefined();
      expect(row?.textContent).toContain(text);
    }, DEADLINE);
  }
}
