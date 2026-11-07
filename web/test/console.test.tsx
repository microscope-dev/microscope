// The console's completions can be chosen without a mouse: Tab and the
// arrow keys walk the open list, and Enter takes one. A click takes one too.
// Run-control lines run the page's own actions and say where the program
// stopped; everything else goes to microscope.

import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";

import { clearConsole } from "../src/console";
import type { Inferior } from "../src/protocol";
import { Console } from "../src/ui/Console";
import type { FakeServer } from "./fake-server";
import { mountPane, stoppedState, unmountPane } from "./pane";

afterEach(() => {
  unmountPane();
  clearConsole();
});

const field = () => page.getByRole("textbox", { name: "Console" });
const option = (name: string) => page.getByRole("option", { name: new RegExp(`^${name}`, "u") });
const entries = () => page.getByTestId("console").getByRole("listitem");

async function completeVa() {
  const server = await mountPane(<Console />);
  await userEvent.type(field(), "va");
  await userEvent.keyboard("{Tab}");
  server.answer(await server.next("complete"), {
    start: 0,
    items: [
      { label: "value", kind: "variable" },
      { label: "vars", kind: "variable" },
    ],
  });
  await expect.element(page.getByRole("listbox", { name: "Completions" })).toBeVisible();
}

it("walks the completions with the arrow keys and takes one with Enter", async () => {
  await completeVa();
  await userEvent.keyboard("{ArrowDown}{ArrowDown}");
  await expect
    .element(page.getByRole("option", { name: /vars/u }))
    .toHaveAttribute("aria-selected", "true");
  const active = field().element().getAttribute("aria-activedescendant");
  expect(active && document.querySelector(`#${CSS.escape(active)}`)?.textContent).toContain("vars");
  await userEvent.keyboard("{Enter}");
  await expect.element(field()).toHaveValue("vars");
  await expect.element(page.getByRole("listbox")).not.toBeInTheDocument();
});

it("cycles the open completions with Tab and Shift+Tab, and Enter only fills the field", async () => {
  const server = await mountPane(<Console />);
  await userEvent.type(field(), "dis");
  await userEvent.keyboard("{Tab}");
  server.answer(await server.next("complete"), {
    start: 0,
    items: [
      { label: "disable", kind: "keyword" },
      { label: "display", kind: "keyword" },
      { label: "disassemble", kind: "keyword" },
    ],
  });
  await expect.element(page.getByRole("listbox", { name: "Completions" })).toBeVisible();
  await expect.element(option("disable")).toHaveAttribute("aria-selected", "false");

  await userEvent.keyboard("{Tab}{Tab}{Tab}");
  await expect.element(option("disassemble")).toHaveAttribute("aria-selected", "true");
  // Past the last, Tab comes round to the first, and Shift+Tab goes back.
  await userEvent.keyboard("{Tab}");
  await expect.element(option("disable")).toHaveAttribute("aria-selected", "true");
  await userEvent.keyboard("{Shift>}{Tab}{/Shift}");
  await expect.element(option("disassemble")).toHaveAttribute("aria-selected", "true");
  await expect.element(field()).toHaveFocus();

  await userEvent.keyboard("{Enter}");
  await expect.element(field()).toHaveValue("disassemble");
  await expect.element(page.getByRole("listbox")).not.toBeInTheDocument();
  expect(server.requests.filter((request) => request.method === "complete")).toHaveLength(1);
  expect(server.requests.map((request) => request.method)).not.toContain("console");
  expect(entries().all()).toHaveLength(0);
});

it("takes a clicked completion and keeps typing in the field", async () => {
  await completeVa();
  await userEvent.click(page.getByRole("option", { name: /value/u }));
  await expect.element(field()).toHaveValue("value");
  await expect.element(field()).toHaveFocus();
});

async function type(line: string) {
  await userEvent.type(field(), line ? `${line}{Enter}` : "{Enter}");
}

/** The program as the server says it is after a run-control request. */
function moved(server: FakeServer, inferior: Inferior) {
  server.send({ type: "state", ...stoppedState({ revision: 2, inferior }) });
}

const at = (stop: number, line: number): Extract<Inferior, { state: "stopped" }> => ({
  state: "stopped",
  pid: 100,
  stop,
  thread: 1,
  frame: 0,
  reason: { kind: "step", description: "stepped over" },
  place: { address: "0x401010", function: "main", path: "/work/basic.c", line },
});

it("steps with n, as the toolbar does, and says where the program stopped", async () => {
  const server = await mountPane(<Console />);
  await type("n");
  const step = await server.next("step");
  expect(step.params).toEqual({ stop: 1, thread: 1, task: null, frame: 0, kind: "over" });
  expect(server.requests.some((request) => request.method === "console")).toBe(false);
  moved(server, at(2, 4));
  server.answer(step, null);
  await expect.element(entries().first()).toMatchTextContent("stop #2 · stepped over · basic.c:4");
});

it("says the program runs until it stops", async () => {
  const server = await mountPane(<Console />);
  await type("c");
  const going = await server.next("continue");
  expect(going.params).toEqual({ stop: 1 });
  moved(server, { state: "running", pid: 100 });
  server.answer(going, null);
  await expect.element(entries().first()).toMatchTextContent("running…");
  moved(server, { ...at(5, 9), reason: { kind: "breakpoint", description: "breakpoint 1" } });
  await expect.element(entries().first()).toMatchTextContent("stop #5 · breakpoint 1 · basic.c:9");
  await expect.element(entries().first()).not.toMatchTextContent("running…");
});

it("repeats the last step on an empty line, and nothing else", async () => {
  const server = await mountPane(<Console />);
  await type("s");
  server.answer(await server.next("step"), null);
  await type("");
  const again = await server.next("step");
  expect(again.params).toMatchObject({ kind: "into" });
  server.answer(again, null);
  await expect.poll(() => entries().all().length).toBe(2);

  await type("p x");
  server.answer(await server.next("console"), { output: "1", row: null });
  await type("");
  await expect.poll(() => entries().all().length).toBe(3);
  expect(server.requests.filter((request) => request.method === "console")).toHaveLength(1);
});

it("refuses a step while the program runs, without asking microscope", async () => {
  const server = await mountPane(
    <Console />,
    {},
    stoppedState({ inferior: { state: "running", pid: 100 } }),
  );
  await type("next");
  await expect.element(entries().first()).toMatchTextContent("the program is not stopped");
  expect(server.requests.map((request) => request.method)).not.toContain("step");
  expect(server.requests.map((request) => request.method)).not.toContain("console");
});

it("starts a program with run, and continue from not started", async () => {
  const notStarted = stoppedState({ inferior: { state: "notStarted" } });
  const server = await mountPane(<Console />, { at: null }, notStarted);
  await type("r");
  expect((await server.next("continue")).params).toEqual({ stop: null });
});

it("finishes the frame shown", async () => {
  const server = await mountPane(<Console />, { at: { stop: 1, thread: 1, frame: 2 } });
  await type("finish");
  expect((await server.next("step")).params).toMatchObject({ kind: "out", frame: 2 });
});

it("advances the thread shown to a location", async () => {
  const server = await mountPane(<Console />, { at: { stop: 1, thread: 1, frame: 1 } });
  await type("until basic.c:9");
  const advance = await server.next("advance");
  expect(advance.params).toEqual({
    stop: 1,
    thread: 1,
    task: null,
    frame: 1,
    location: "basic.c:9",
  });
  moved(server, at(3, 9));
  server.answer(advance, null);
  await expect.element(entries().first()).toMatchTextContent("stop #3");
});

it("says how to use until without a location", async () => {
  const server = await mountPane(<Console />);
  await type("until");
  await expect.element(entries().first()).toMatchTextContent("until runs to a location");
  expect(server.requests.map((request) => request.method)).not.toContain("advance");
});

it("steps into the call it names, and lists the line's calls when it names none", async () => {
  const server = await mountPane(<Console />);
  const calls = {
    calls: [
      { call: "0x401020", callee: "hash", target: "0x401100" },
      { call: "0x401030", callee: "strcmp", target: null },
    ],
  };
  await type("step strcmp");
  server.answer(await server.next("stepTargets"), calls);
  expect((await server.next("step")).params).toMatchObject({ kind: "into", call: "0x401030" });

  await type("s *0x401020");
  server.answer(await server.next("stepTargets"), calls);
  expect((await server.next("step")).params).toMatchObject({ call: "0x401020" });

  await type("s missing");
  server.answer(await server.next("stepTargets"), calls);
  await expect
    .element(entries().last())
    .toMatchTextContent("this line does not call missing; it calls hash, strcmp");
});

it("leaves expressions that start with a command's name to microscope", async () => {
  const server = await mountPane(<Console />);
  await type("n + 1");
  expect((await server.next("console")).params).toMatchObject({ line: "n + 1" });
  expect(server.requests.map((request) => request.method)).not.toContain("step");
});

it("clears with cls and with Ctrl+L", async () => {
  const server = await mountPane(<Console />);
  await type("p x");
  server.answer(await server.next("console"), { output: "1", row: null });
  await expect.poll(() => entries().all().length).toBe(1);
  await type("cls");
  await expect.poll(() => entries().all().length).toBe(0);

  await type("p x");
  server.answer(await server.next("console"), { output: "1", row: null });
  await expect.poll(() => entries().all().length).toBe(1);
  await userEvent.keyboard("{Control>}l{/Control}");
  await expect.poll(() => entries().all().length).toBe(0);
});

it("adds the page's run-control commands to help", async () => {
  const server = await mountPane(<Console />);
  await type("help");
  server.answer(await server.next("console"), { output: "print, p  Print a value", row: null });
  await expect.element(entries().first()).toMatchTextContent("print, p");
  await expect.element(entries().first()).toMatchTextContent("advance, until LOCATION");
});

it("shows a function's code in Disassembly instead of listing it", async () => {
  const server = await mountPane(<Console />);
  await type("disas table_find");
  const resolve = await server.next("resolve");
  expect(resolve.params).toEqual({ name: "table_find" });
  server.answer(resolve, { address: "0x401200" });
  await expect
    .element(entries().first())
    .toMatchTextContent("table_find at 0x401200, in Disassembly");
  expect(server.requests.map((request) => request.method)).not.toContain("console");

  await type("disassemble");
  await expect.element(entries().last()).toMatchTextContent("the frame's code, in Disassembly");
  await type("disas nothing");
  server.refuse(await server.next("resolve"), "failed", "no symbol named 'nothing' was found");
  await expect.element(entries().last()).toMatchTextContent("no symbol named 'nothing'");
  expect(server.requests.map((request) => request.method)).not.toContain("console");
});

it("leaves a disassembly with an instruction count to microscope", async () => {
  const server = await mountPane(<Console />);
  await type("disas main 8");
  expect((await server.next("console")).params).toMatchObject({ line: "disas main 8" });
});
