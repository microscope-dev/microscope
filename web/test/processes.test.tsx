// The process switcher beside the stop banner: hidden for one process,
// and for several, each with how it joined and where it is, choosing one.

import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import type { Group, Member } from "../src/protocol";
import { Processes } from "../src/ui/Processes";
import { mountPane, stoppedState, unmountPane } from "./pane";

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

it("is hidden while the program has one process", async () => {
  await mountPane(
    <Processes />,
    {},
    stoppedState({ group: { ...two, members: [member({ number: 1 })], selected: 1 } }),
  );
  await expect.element(page.getByRole("navigation", { name: "Processes" })).not.toBeInTheDocument();
});

it("lists each process, how it joined, and where it is, and selects one", async () => {
  const server = await mountPane(<Processes />, {}, stoppedState({ group: two }));
  const child = page.getByRole("button", { name: /\[2\] tree/ });
  await expect.element(child).toHaveAttribute("aria-pressed", "true");
  expect(child.element().textContent).toContain("forked by [1]");
  expect(child.element().textContent).toContain("◂ stopped here");
  // A process that exited has nothing to show.
  await expect.element(page.getByRole("button", { name: /\[3\] tree/ })).toBeDisabled();

  await userEvent.click(page.getByRole("button", { name: /\[1\] tree/ }));
  const asked = await server.next("selectMember");
  expect(asked.params).toEqual({ member: 1 });
  server.answer(asked, null);

  await userEvent.click(page.getByRole("button", { name: "Detach [2]" }));
  expect((await server.next("detachMember")).params).toEqual({ member: 2 });
});
