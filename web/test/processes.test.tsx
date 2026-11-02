// The Processes pane: one process says so, and for several, each with how it
// joined and where it is, choosing one.

import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import type { Group, Member } from "../src/protocol";
import { Processes } from "../src/ui/Processes";
import { mountPane, stoppedState, unmountPane } from "./pane";
import "../src/styles.css";

afterEach(unmountPane);

function member(changes: Partial<Member> & { number: number }): Member {
  return {
    pid: 100 + changes.number,
    parent: null,
    command: "/work/tree 2 direct serial",
    joined: "launched",
    program: null,
    state: "stopped",
    description: "paused",
    ...changes,
  };
}

const two: Group = {
  members: [
    member({ number: 1 }),
    member({
      number: 2,
      parent: 1,
      joined: "forked",
      description: "breakpoint 1 (hit 1) in child_work",
    }),
    member({ number: 3, parent: 1, joined: "forked", state: "exited", description: "exited" }),
  ],
  selected: 2,
  stopped: 2,
};

it("says so while the program has one process", async () => {
  await mountPane(
    <Processes />,
    {},
    stoppedState({ group: { ...two, members: [member({ number: 1 })], selected: 1 } }),
  );
  await expect.element(page.getByText("This session debugs one process.")).toBeVisible();
  await expect.element(page.getByRole("navigation", { name: "Processes" })).not.toBeInTheDocument();
});

it("lists each process, how it joined, and where it is, and selects one", async () => {
  const server = await mountPane(<Processes />, {}, stoppedState({ group: two }));
  const child = page.getByRole("button", { name: /\[2\] tree/ });
  await expect.element(child).toHaveAttribute("aria-pressed", "true");
  expect(child.element().textContent).toContain("forked by [1]");
  expect(child.element().textContent).toContain("◂ stopped here");
  await userEvent.click(page.getByText("Other processes (1)", { exact: true }));
  // A process that exited has nothing to show.
  await expect.element(page.getByRole("button", { name: /\[3\] tree/ })).toBeDisabled();

  await userEvent.click(page.getByRole("button", { name: /\[1\] tree/ }));
  const asked = await server.next("selectMember");
  expect(asked.params).toEqual({ member: 1 });
  server.answer(asked, null);

  await userEvent.click(page.getByRole("button", { name: "Detach [2]" }));
  expect((await server.next("detachMember")).params).toEqual({ member: 2 });
});

it.each([1440, 390])("folds a long process history and stays within a %ipx pane", async (width) => {
  const history = Array.from({ length: 35 }, (_, index) =>
    member({
      number: index + 3,
      parent: 1,
      joined: "forked",
      state: index % 2 ? "exited" : "watched",
    }),
  );
  const server = await mountPane(
    <div style={{ width: `${width}px`, display: "flex", flexDirection: "column", height: "400px" }}>
      <Processes />
    </div>,
    {},
    stoppedState({ group: { ...two, members: [...two.members.slice(0, 2), ...history] } }),
  );
  const switcher = page.getByRole("navigation", { name: "Processes" });
  const disclosure = page.getByText("Other processes (35)", { exact: true });
  await expect.element(disclosure).toBeVisible();
  await expect.element(page.getByRole("button", { name: /\[3\] tree/ })).not.toBeInTheDocument();
  await disclosure.element().focus();
  await userEvent.keyboard("{Enter}");
  await expect.element(page.getByRole("button", { name: /\[37\] tree/ })).toBeDisabled();
  expect(switcher.element().scrollWidth).toBe(switcher.element().clientWidth);
  // The pane scrolls within its own height.
  expect(switcher.element().getBoundingClientRect().height).toBeLessThanOrEqual(400);

  await userEvent.click(page.getByRole("button", { name: /\[1\] tree/ }));
  const asked = await server.next("selectMember");
  expect(asked.params).toEqual({ member: 1 });
  server.answer(asked, null);
  await userEvent.click(page.getByRole("button", { name: "Detach [2]" }));
  expect((await server.next("detachMember")).params).toEqual({ member: 2 });
});

it("lists many selectable processes without folding them into history", async () => {
  const members = Array.from({ length: 37 }, (_, index) =>
    member({ number: index + 1, state: index % 2 ? "running" : "stopped" }),
  );
  const server = await mountPane(
    <div style={{ width: "390px", display: "flex", flexDirection: "column", height: "200px" }}>
      <Processes />
    </div>,
    {},
    stoppedState({ group: { members, selected: 1, stopped: 1 } }),
  );
  const switcher = page.getByRole("navigation", { name: "Processes" });
  await expect.element(switcher).toBeVisible();
  expect(switcher.element().scrollHeight).toBeGreaterThan(switcher.element().clientHeight);
  expect(switcher.element().querySelector("details")).toBeNull();
  await userEvent.click(page.getByRole("button", { name: /\[37\] tree/ }));
  expect((await server.next("selectMember")).params).toEqual({ member: 37 });
});

it("keeps a selected exited process outside the folded entries", async () => {
  await mountPane(<Processes />, {}, stoppedState({ group: { ...two, selected: 3, stopped: 3 } }));
  const selected = page.getByRole("button", { name: /\[3\] tree/ });
  await expect.element(selected).toHaveAttribute("aria-pressed", "true");
  await expect.element(selected).toBeDisabled();
  expect(selected.element().textContent).toContain("stopped here");
});
