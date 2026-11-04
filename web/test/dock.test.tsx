// The dock as a person uses it, with panes of its own: tabs choose and close
// by pointer and by key, separators resize, a pane that throws or lacks what
// it needs says so and leaves the others working, and choosing a tab renders
// nothing outside its group.

import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";

import "../src/styles.css";
import { Dock } from "../src/workbench/Dock";
import { kinds, type PaneKind, type PaneProps } from "../src/workbench/kinds";
import { build, type KindId, type Pane } from "../src/workbench/layout";
import { workbench } from "../src/workbench/store";
import { useVisible } from "../src/workbench/visible";

afterEach(cleanup);

/** How often each pane's body rendered. */
const renders = new Map<string, number>();

function Counting({ pane }: PaneProps) {
  renders.set(pane.key, (renders.get(pane.key) ?? 0) + 1);
  const visible = useVisible();
  return <p>{`${pane.key} ${visible ? "shown" : "hidden"}`}</p>;
}

function Throwing(): never {
  throw new Error("the answer was malformed");
}

const kind = (id: string, more: Partial<PaneKind<string>> = {}): PaneKind<string> => ({
  id: id as KindId,
  title: (pane) => pane.key,
  Body: Counting,
  needs: null,
  keepAlive: true,
  closable: () => true,
  ...more,
});

const KINDS = kinds([
  kind("plain"),
  kind("broken", { Body: Throwing }),
  kind("needy", { needs: "stop" }),
  kind("fixed", { closable: () => false }),
]);

const pane = (kindId: string, key: string): Pane => ({ kind: kindId as KindId, key, params: null });

async function mount(shape: Parameters<typeof build>[0]) {
  await page.viewport(1000, 600);
  renders.clear();
  workbench.setState({ layout: build(shape) });
  render(
    <div style={{ display: "flex", width: "900px", height: "500px" }}>
      <Dock
        setup={{
          kinds: KINDS,
          useMissing: (need) => (need === "stop" ? "The program is not stopped." : null),
        }}
      />
    </div>,
  );
}

const tab = (name: string) => page.getByRole("tab", { name, exact: true });

it("shows one pane per group, keeping the others mounted and hidden", async () => {
  await mount({
    axis: "row",
    children: [
      [1, [pane("plain", "Threads"), pane("plain", "Tasks")]],
      [1, [pane("plain", "Registers")]],
    ],
  });
  await expect.element(page.getByText("Threads shown")).toBeVisible();
  await expect.element(page.getByText("Tasks hidden", { exact: true })).toBeInTheDocument();
  await expect.element(tab("Threads")).toHaveAttribute("aria-selected", "true");

  await userEvent.click(tab("Tasks"));
  await expect.element(page.getByText("Tasks shown")).toBeVisible();
  await expect.element(page.getByText("Threads hidden")).not.toBeVisible();
  await expect.element(tab("Tasks")).toHaveAttribute("aria-selected", "true");
});

it("renders nothing outside the group whose tab is chosen", async () => {
  await mount({
    axis: "row",
    children: [
      [1, [pane("plain", "Threads"), pane("plain", "Tasks")]],
      [1, [pane("plain", "Registers"), pane("plain", "Memory")]],
    ],
  });
  await expect.element(page.getByText("Registers shown")).toBeVisible();
  const before = { registers: renders.get("Registers"), memory: renders.get("Memory") };
  await userEvent.click(tab("Tasks"));
  await expect.element(page.getByText("Tasks shown")).toBeVisible();
  expect({ registers: renders.get("Registers"), memory: renders.get("Memory") }).toEqual(before);
});

it("walks tabs with the arrow keys and closes one with Delete", async () => {
  await mount([pane("plain", "Console"), pane("plain", "Output"), pane("fixed", "Signals")]);
  await userEvent.click(tab("Console"));
  await userEvent.keyboard("{ArrowRight}");
  await expect.element(tab("Output")).toHaveAttribute("aria-selected", "true");
  await expect.element(tab("Output")).toHaveFocus();
  await userEvent.keyboard("{End}");
  await expect.element(tab("Signals")).toHaveAttribute("aria-selected", "true");
  // A tab that stays open ignores Delete.
  await userEvent.keyboard("{Delete}");
  await expect.element(tab("Signals")).toBeInTheDocument();
  await userEvent.keyboard("{Home}{Delete}");
  await expect.element(tab("Console")).not.toBeInTheDocument();
  await expect.element(tab("Output")).toHaveAttribute("aria-selected", "true");
  await expect.element(tab("Output")).toHaveFocus();
  await userEvent.keyboard("{ArrowRight}");
  await expect.element(tab("Signals")).toHaveFocus();
  await expect.element(page.getByRole("button", { name: "Close Signals" })).not.toBeInTheDocument();
});

it("closes the last tab of a group, and the group with it", async () => {
  await mount({
    axis: "row",
    children: [
      [1, [pane("plain", "Threads")]],
      [1, [pane("plain", "Registers")]],
    ],
  });
  await userEvent.click(page.getByRole("button", { name: "Close Registers" }));
  await expect.element(tab("Registers")).not.toBeInTheDocument();
  expect(page.getByRole("tablist").elements()).toHaveLength(1);
  expect(page.getByRole("separator").elements()).toHaveLength(0);
});

it("says why a pane is empty, and keeps working when one throws", async () => {
  await mount({
    axis: "row",
    children: [
      [1, [pane("broken", "Variables")]],
      [1, [pane("needy", "Registers")]],
      [1, [pane("plain", "Threads")]],
    ],
  });
  const failed = page.getByRole("alert");
  await expect.element(failed.getByText("Variables stopped working.")).toBeVisible();
  await expect.element(failed.getByText(/the answer was malformed/)).toBeVisible();
  await expect.element(page.getByText("The program is not stopped.")).toBeVisible();
  await expect.element(page.getByText("Threads shown")).toBeVisible();
});

it("resizes a split by keyboard, and evens it out with Enter", async () => {
  await mount({
    axis: "row",
    children: [
      [1, [pane("plain", "Threads")]],
      [1, [pane("plain", "Registers")]],
    ],
  });
  const separator = page.getByRole("separator", { name: "Resize Threads and Registers" });
  await expect.element(separator).toHaveAttribute("aria-valuenow", "50");
  const threads = () =>
    (document.querySelector('[data-group] [role="tablist"]') as HTMLElement).getBoundingClientRect()
      .width;
  const wide = threads();
  (separator.element() as HTMLElement).focus();
  await userEvent.keyboard("{ArrowRight}{ArrowRight}");
  await expect.element(separator).not.toHaveAttribute("aria-valuenow", "50");
  expect(threads()).toBeGreaterThan(wide + 20);
  await userEvent.keyboard("{Home}");
  // A pane keeps its least width.
  expect(threads()).toBeGreaterThanOrEqual(119);
  await userEvent.keyboard("{Enter}");
  await expect.element(separator).toHaveAttribute("aria-valuenow", "50");
});
