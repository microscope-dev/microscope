import type { User } from "./user";

export async function breakStepAndRead(user: User): Promise<void> {
  await user.addBreakpoint("kvstore.c:92");
  await user.press("F5");
  await user.expectCurrentLine("int status = 0;");
  await user.press("F10");
  await user.expectCurrentLine("switch (req->op) {");
  await user.expectValue("Variables", "status", "0");
}

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
