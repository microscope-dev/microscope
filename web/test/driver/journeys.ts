// Journeys through the page, written once against `User`. Each names the
// program it expects, which each backend provides: the real fixture, or a
// script of it (scripted.ts).

import type { User } from "./user";

/** In kvstore, breaks where handle_request starts its answer, steps over
 * that line, and reads the local it set. */
export async function breakStepAndRead(user: User): Promise<void> {
  await user.addBreakpoint("kvstore.c:92");
  await user.press("F5");
  await user.expectCurrentLine("int status = 0;");
  await user.press("F10");
  await user.expectCurrentLine("switch (req->op) {");
  await user.expectValue("Variables", "status", "0");
}

/** The Debug layout stopped in kvstore's handle_request. */
const DEBUG = (documents: string[], bottom: string[]) => [
  ["*Threads", "Tasks", "Processes"],
  ["*Call Stack", "Stops"],
  ["*Breakpoints"],
  documents,
  bottom,
  ["*Watch"],
  ["*Variables"],
  ["*Registers"],
];

/** In kvstore, stops where handle_request starts its answer, and moves among
 * the panes by tab, key, and menu, closing one and bringing it back. */
export async function switchPanes(user: User): Promise<void> {
  await user.addBreakpoint("kvstore.c:92");
  await user.press("F5");
  await user.expectCurrentLine("int status = 0;");
  const source = ["*kvstore.c", "Disassembly", "Memory", "Drawings"];
  const output = ["*Output", "Console", "Signals", "Modules"];
  await user.expectGroups(DEBUG(source, output));

  await user.chooseTab("Signals");
  await user.press("Alt+D");
  await user.expectGroups(
    DEBUG(
      ["kvstore.c", "*Disassembly", "Memory", "Drawings"],
      ["Output", "Console", "*Signals", "Modules"],
    ),
  );
  await user.press("Escape");
  await user.expectGroups(DEBUG(source, ["Output", "Console", "*Signals", "Modules"]));

  await user.menu("View", "Console");
  await user.expectGroups(DEBUG(source, ["Output", "*Console", "Signals", "Modules"]));
  await user.press("Escape");
  await user.press("Alt+W");
  await user.expectGroups(DEBUG(source, ["Output", "*Signals", "Modules"]));
  await user.press("Alt+6");
  await user.expectGroups(DEBUG(source, ["Output", "Signals", "*Console", "Modules"]));
}
