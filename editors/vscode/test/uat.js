"use strict";

const assert = require("assert");
const childProcess = require("child_process");
const fs = require("fs");
const path = require("path");
const vscode = require("vscode");

const root = process.env.MICROSCOPE_UAT_ROOT;
const record = process.env.MICROSCOPE_UAT_RECORD;
const fixture = (name) => path.join(root, "build/test-programs", name);
const source = (name) => path.join(root, "tests/fixtures/c", name);
const extension = () => require(path.join(root, "editors/vscode/extension.js"));
const TIMEOUT = 15000;

const transcripts = new Map();
let waiters = [];

vscode.debug.registerDebugAdapterTrackerFactory("microscope", {
  createDebugAdapterTracker(session) {
    const lines = [];

    let name =
      session.parentSession === undefined
        ? session.configuration.name
        : `${session.parentSession.configuration.name}.child`;
    while (transcripts.has(name)) {
      name = `${name}.restarted`;
    }
    transcripts.set(name, lines);
    const notify = (direction, packet) => {
      waiters = waiters.filter((waiter) => !waiter(session, packet, direction));
    };
    return {
      onWillReceiveMessage: (packet) => {
        lines.push(`<- ${JSON.stringify(packet)}`);
        notify("client", packet);
      },
      onDidSendMessage: (packet) => {
        lines.push(`-> ${JSON.stringify(packet)}`);
        notify("adapter", packet);
      },
      onError: (error) => lines.push(`!! ${error}`),
    };
  },
});

function message(description, predicate, from = "adapter") {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`timed out waiting for ${description}`)),
      TIMEOUT,
    );
    waiters.push((session, sent, direction) => {
      if (direction !== from || !predicate(session, sent)) {
        return false;
      }
      clearTimeout(timer);
      resolve({ session, message: sent });
      return true;
    });
  });
}

const event = (name, predicate = () => true) =>
  message(
    `a ${name} event`,
    (_, sent) => sent.type === "event" && sent.event === name && predicate(sent.body ?? {}),
  );

const response = (command, predicate = () => true) =>
  message(
    `a ${command} response`,
    (_, sent) => sent.type === "response" && sent.command === command && predicate(sent),
  );

const request = (command, predicate = () => true) =>
  message(
    `VS Code to send ${command}`,
    (_, sent) =>
      sent.type === "request" && sent.command === command && predicate(sent.arguments ?? {}),
    "client",
  );

async function answer(command, predicate = () => true) {
  let seq;
  const sent = request(command, (arguments_) => predicate(arguments_));
  const answered = message(
    `the answer to ${command}`,
    (_, reply) =>
      reply.type === "response" &&
      reply.command === command &&
      seq !== undefined &&
      reply.request_seq === seq,
  );
  seq = (await sent).message.seq;
  return (await answered).message;
}

function within(description, promise) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`timed out waiting for ${description}`)), TIMEOUT);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

function until(description, predicate) {
  return within(
    description,
    new Promise((resolve) => {
      const poll = () => (predicate() ? resolve() : setTimeout(poll, 20));
      poll();
    }),
  );
}

function terminated(name) {
  return within(
    "the session to end",
    new Promise((resolve) => {
      const listener = vscode.debug.onDidTerminateDebugSession((session) => {
        if (name === undefined || session.name === name) {
          listener.dispose();
          resolve(session);
        }
      });
    }),
  );
}

function save() {
  fs.mkdirSync(record, { recursive: true });
  for (const [name, lines] of transcripts) {
    const file = name.replaceAll(/[^A-Za-z0-9._-]+/gu, "-");
    fs.writeFileSync(path.join(record, `${file}.log`), `${lines.join("\n")}\n`);
  }
}

async function remove(breakpoint) {
  const command =
    breakpoint instanceof vscode.FunctionBreakpoint ? "setFunctionBreakpoints" : "setBreakpoints";
  const applied = response(command);
  vscode.debug.removeBreakpoints([breakpoint]);
  await applied;
}

let focused;

function focus() {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("timed out waiting for a focused frame")),
      TIMEOUT,
    );
    const check = (item) => {
      if (
        !(item instanceof vscode.DebugStackFrame) ||
        `${item.session.id} ${item.frameId}` === focused
      ) {
        return false;
      }
      focused = `${item.session.id} ${item.frameId}`;
      clearTimeout(timer);
      resolve(item);
      return true;
    };
    if (check(vscode.debug.activeStackItem)) {
      return;
    }
    const listener = vscode.debug.onDidChangeActiveStackItem((item) => {
      if (check(item)) {
        listener.dispose();
      }
    });
  });
}

function lineOf(file, marker) {
  const index = fs
    .readFileSync(file, "utf8")
    .split("\n")
    .findIndex((line) => line.includes(marker));
  assert.notStrictEqual(index, -1, `${marker} in ${file}`);
  return index;
}

function sourceBreakpoint(file, marker, options = {}) {
  const location = new vscode.Location(
    vscode.Uri.file(file),
    new vscode.Position(lineOf(file, marker), 0),
  );
  return new vscode.SourceBreakpoint(
    location,
    true,
    options.condition,
    options.hitCondition,
    options.logMessage,
  );
}

async function topFrame(session, threadId) {
  const trace = await session.customRequest("stackTrace", { threadId, startFrame: 0, levels: 1 });
  return trace.stackFrames[0];
}

const workspace = () => vscode.workspace.workspaceFolders[0];

async function launchToStop(configuration, reason) {
  const stopped = event("stopped", (body) => reason === undefined || body.reason === reason);
  assert.ok(
    await vscode.debug.startDebugging(undefined, {
      type: "microscope",
      request: "launch",
      cwd: root,
      ...configuration,
    }),
  );
  const { session, message: sent } = await stopped;

  const frame = reason === "entry" ? await focusThread(sent.body.threadId) : await focus();
  return { session, threadId: sent.body.threadId, frame, body: sent.body };
}

function focusThread(threadId) {
  return until("a focused thread", () => {
    const item = vscode.debug.activeStackItem;
    return (
      (item instanceof vscode.DebugThread || item instanceof vscode.DebugStackFrame) &&
      item.threadId === threadId
    );
  });
}

async function finish(exitCode = 0) {
  for (const breakpoint of vscode.debug.breakpoints) {
    await remove(breakpoint);
  }
  const exited = event("exited");
  const ended = terminated();
  await vscode.commands.executeCommand("workbench.action.debug.continue");
  assert.strictEqual((await exited).message.body.exitCode, exitCode);
  await ended;
}

async function stop(session) {
  const ended = terminated(session.name);
  await vscode.debug.stopDebugging(session);
  await ended;
}

async function onSelection(command, text, context) {
  const document = await vscode.workspace.openTextDocument({ content: text });
  const editor = await vscode.window.showTextDocument(document, { preview: true });
  editor.selection = new vscode.Selection(0, 0, 0, text.length);
  const answered = answer(
    "evaluate",
    (arguments_) => arguments_.expression === text && arguments_.context === context,
  );
  await vscode.commands.executeCommand(command);
  const result = await answered;
  await vscode.commands.executeCommand("workbench.action.revertAndCloseActiveEditor");
  return result;
}

// eslint-disable-next-line no-control-regex
const plain = (styled) => styled?.replaceAll(/\u001B\[[0-9;]*m/gu, "");

async function inConsole(text) {
  const reply = await onSelection("editor.debug.action.selectionToRepl", text, "repl");
  if (reply.body?.result !== undefined) {
    reply.body.result = plain(reply.body.result);
  }
  reply.message = plain(reply.message);
  return reply;
}

async function createALaunchJson() {
  const folder = workspace().uri.fsPath;
  const program = path.join(folder, "main.c");
  const launchJson = path.join(folder, ".vscode", "launch.json");
  fs.writeFileSync(program, "int main(void) { return 0; }\n");
  try {
    await vscode.window.showTextDocument(vscode.Uri.file(program));
    await vscode.commands.executeCommand("workbench.action.debug.start");
    await until("launch.json", () => fs.existsSync(launchJson));

    const text = fs.readFileSync(launchJson, "utf8").replaceAll(/^\s*\/\/.*$/gmu, "");
    const { configurations } = JSON.parse(text);
    assert.deepStrictEqual(
      configurations.map(({ type, request: kind }) => ({ type, request: kind })),
      [{ type: "microscope", request: "launch" }],
    );
    assert.strictEqual(vscode.debug.activeDebugSession, undefined);
  } finally {
    await vscode.commands.executeCommand("workbench.action.closeAllEditors");
    fs.rmSync(path.join(folder, ".vscode"), { recursive: true, force: true });
    fs.rmSync(program, { force: true });
  }
}

const launchBasic = (name) => ({
  type: "microscope",
  request: "launch",
  name,
  program: fixture("basic"),
  cwd: root,
});

async function findTheAdapterFromTheSetting() {
  const folder = workspace();
  const wrapper = path.join(folder.uri.fsPath, "microscope-wrapper");
  const marker = `${wrapper}.used`;
  fs.writeFileSync(wrapper, `#!/bin/sh\ntouch '${marker}'\nexec microscope "$@"\n`, {
    mode: 0o755,
  });
  const settings = vscode.workspace.getConfiguration("microscope");
  try {
    await settings.update(
      "path",
      "${workspaceFolder}/missing-microscope",
      vscode.ConfigurationTarget.Global,
    );
    await assert.rejects(
      vscode.debug.startDebugging(folder, launchBasic("vscode-missing-adapter")),
      /Cannot find microscope at "\$\{workspaceFolder\}\/missing-microscope"/u,
    );
    await settings.update("path", 7, vscode.ConfigurationTarget.Global);
    await assert.rejects(
      vscode.debug.startDebugging(folder, launchBasic("vscode-invalid-adapter")),
      /Cannot find microscope at "7"/u,
    );

    await settings.update(
      "path",
      "${workspaceFolder}/microscope-wrapper",
      vscode.ConfigurationTarget.Global,
    );
    const exited = event("exited");
    const ended = terminated("vscode-setting");
    assert.ok(await vscode.debug.startDebugging(folder, launchBasic("vscode-setting")));
    assert.strictEqual((await exited).message.body.exitCode, 0);
    await ended;
    assert.ok(fs.existsSync(marker), "the session ran the configured adapter");
  } finally {
    await settings.update("path", undefined, vscode.ConfigurationTarget.Global);
    fs.rmSync(wrapper, { force: true });
    fs.rmSync(marker, { force: true });
  }
}

async function logTheProtocolToAFile() {
  const log = path.join(workspace().uri.fsPath, "microscope-dap.log");
  const settings = vscode.workspace.getConfiguration("microscope");
  try {
    await settings.update(
      "logFile",
      "${workspaceFolder}/microscope-dap.log",
      vscode.ConfigurationTarget.Global,
    );
    const exited = event("exited");
    const ended = terminated("vscode-log");
    assert.ok(
      await vscode.debug.startDebugging(workspace(), {
        type: "microscope",
        request: "launch",
        name: "vscode-log",
        program: fixture("basic"),
      }),
    );
    await exited;
    await ended;
    const text = fs.readFileSync(log, "utf8");
    assert.match(text, /"command":"initialize"/u);
    assert.match(text, /"event":"exited"/u);
  } finally {
    await settings.update("logFile", undefined, vscode.ConfigurationTarget.Global);
    fs.rmSync(log, { force: true });
  }
}

async function launchStepInspectAndRestart() {
  const breakpoint = sourceBreakpoint(source("basic.c"), "return microscope_value;");
  vscode.debug.addBreakpoints([breakpoint]);
  const stopped = event("stopped", (body) => body.reason === "breakpoint");
  assert.ok(
    await vscode.debug.startDebugging(undefined, {
      type: "microscope",
      request: "launch",
      name: "vscode-launch",
      program: fixture("basic"),
      cwd: root,
    }),
  );
  const { session, message: sent } = await stopped;
  const threadId = sent.body.threadId;

  await focus();
  let frame = await topFrame(session, threadId);
  assert.strictEqual(frame.name, "breakpoint_target");
  assert.strictEqual(frame.line, lineOf(source("basic.c"), "return microscope_value;") + 1);
  const value = await session.customRequest("evaluate", {
    expression: "microscope_value",
    frameId: frame.id,
    context: "watch",
  });
  assert.strictEqual(value.result, "1234605616436508552");

  let step = event("stopped", (body) => body.reason === "step");
  await vscode.commands.executeCommand("workbench.action.debug.stepOut");
  await step;
  await focus();
  frame = await topFrame(session, threadId);
  assert.strictEqual(frame.name, "main");
  step = event("stopped", (body) => body.reason === "step");
  await vscode.commands.executeCommand("workbench.action.debug.stepOver");
  await step;
  await focus();

  const disassembled = response(
    "disassemble",
    (reply) => reply.success && reply.body.instructions.length > 0,
  );
  await vscode.commands.executeCommand("debug.action.openDisassemblyView");
  await disassembled;

  const again = event("stopped", (body) => body.reason === "breakpoint");
  await vscode.commands.executeCommand("workbench.action.debug.restart");
  await again;
  await focus();

  await remove(breakpoint);
  const exited = event("exited");
  const ended = terminated();
  await vscode.commands.executeCommand("workbench.action.debug.continue");
  assert.strictEqual((await exited).message.body.exitCode, 0);
  await ended;
}

async function stepIntoByInstructionAndRunToCursor() {
  const file = source("basic.c");
  const { session, threadId } = await launchToStop(
    {
      name: "vscode-stepping",
      program: fixture("basic"),
      stopOnEntry: true,
    },
    "entry",
  );
  const editor = await vscode.window.showTextDocument(vscode.Uri.file(file));
  const line = lineOf(file, "uint64_t second = breakpoint_target();");
  editor.selection = new vscode.Selection(line, 4, line, 4);
  let stopped = event("stopped");
  await vscode.commands.executeCommand("editor.debug.action.runToCursor");
  await stopped;
  await focus();
  assert.strictEqual((await topFrame(session, threadId)).line, line + 1);

  const into = request("stepIn");
  stopped = event("stopped", (body) => body.reason === "step");
  await vscode.commands.executeCommand("workbench.action.debug.stepInto");
  assert.notStrictEqual((await into).message.arguments.granularity, "instruction");
  await stopped;
  await focus();
  assert.strictEqual((await topFrame(session, threadId)).name, "breakpoint_target");

  const disassembled = response("disassemble", (reply) => reply.success);
  await vscode.commands.executeCommand("debug.action.openDisassemblyView");
  await disassembled;
  const before = (await topFrame(session, threadId)).instructionPointerReference;
  const next = request("next");
  stopped = event("stopped", (body) => body.reason === "step");
  await vscode.commands.executeCommand("workbench.action.debug.stepOver");
  assert.strictEqual((await next).message.arguments.granularity, "instruction");
  await stopped;
  await focus();
  const after = await topFrame(session, threadId);
  assert.notStrictEqual(after.instructionPointerReference, before);
  assert.strictEqual(after.name, "breakpoint_target");
  await vscode.commands.executeCommand("workbench.action.closeAllEditors");
  await finish();
}

async function conditionsHitCountsLogpointsAndRefusals() {
  const file = source("hit-counts.c");
  const breakpoints = [
    sourceBreakpoint(file, "last_call = call;", { condition: "call == 5" }),
    sourceBreakpoint(file, "counted(call);", { logMessage: "calling {call} after {last_call}" }),
    new vscode.FunctionBreakpoint("caller", true, undefined, ">= 39"),

    sourceBreakpoint(file, "shared(call);", { condition: "call ==" }),
    sourceBreakpoint(file, "shared(call + SECOND_SITE_OFFSET);", { condition: "call = 3" }),
    sourceBreakpoint(file, "for (uint64_t call = 1", { hitCondition: "5" }),
    new vscode.FunctionBreakpoint("no_such_function"),
  ];
  vscode.debug.addBreakpoints(breakpoints);
  const placed = answer("setBreakpoints", (arguments_) => arguments_.source.path === file);
  const functions = answer("setFunctionBreakpoints");
  const logged = event("output", (body) => body.output === "calling 5 after 4\n");
  const { session, threadId, frame } = await launchToStop(
    {
      name: "vscode-breakpoints",
      program: fixture("hit-counts-gcc-o0"),
    },
    "breakpoint",
  );
  const lines = (await placed).body.breakpoints;
  assert.deepStrictEqual(
    lines.slice(0, 2).map((breakpoint) => breakpoint.verified),
    [true, true],
  );
  for (const [breakpoint, reason] of [
    [lines[2], /invalid condition: expected an operand/u],
    [lines[3], /assign/u],
    [lines[4], /a bare count is ambiguous; write ==5 .* or >=5/u],
  ]) {
    assert.strictEqual(breakpoint.verified, false, JSON.stringify(breakpoint));
    assert.match(breakpoint.message, reason);
  }
  const [caller, missing] = (await functions).body.breakpoints;
  assert.strictEqual(caller.verified, true);
  assert.strictEqual(missing.verified, false);
  assert.match(missing.message, /no_such_function/u);

  await logged;
  const call = await session.customRequest("evaluate", {
    expression: "call",
    frameId: frame.frameId,
    context: "watch",
  });
  assert.strictEqual(call.result, "5");
  await remove(breakpoints[0]);
  await remove(breakpoints[1]);

  const hit = event("stopped", (body) => body.reason === "function breakpoint");
  await vscode.commands.executeCommand("workbench.action.debug.continue");
  await hit;
  const at = await focus();
  const count = await session.customRequest("evaluate", {
    expression: "call",
    frameId: at.frameId,
    context: "watch",
  });
  assert.strictEqual(count.result, "39");
  assert.strictEqual((await topFrame(session, threadId)).name, "caller");
  await finish();
}

async function consoleWatchHoverHexadecimalAndInlineValues() {
  const file = source("command-names.c");
  const line = lineOf(file, "volatile int sink");
  vscode.debug.addBreakpoints([sourceBreakpoint(file, "volatile int sink")]);
  await vscode.commands.executeCommand("workbench.view.debug");

  const declared = request("locations");
  const { frame } = await launchToStop(
    { name: "vscode-console", program: fixture("command-names") },
    "breakpoint",
  );
  const editor = await vscode.window.showTextDocument(vscode.Uri.file(file), {
    preserveFocus: false,
  });
  await declared;

  for (const [text, result] of [
    ["x + 1", "21"],
    ["list", "5"],
    ["n", "10"],
    ["p/x list", "(int) list = 0x5"],
    ["(u8)-1", "255"],
    ["$rip == $pc", "true"],
    ["sizeof(origin)", "8"],
  ]) {
    const reply = await inConsole(text);
    assert.ok(reply.success, JSON.stringify(reply));
    assert.strictEqual(reply.body.result, result, text);
  }
  assert.match((await inConsole("bt")).body.result, /^#0 .*names/u);
  assert.match((await inConsole("whatis where")).body.result, /point \*/u);

  for (const [text, reason] of [
    ["next", /`next` is not available in the debug console/u],
    ["x + missing", /missing[\s\S]*\^{7}/u],
    ["origin + 1", /type/u],
    ["1 / 0", /divi/u],
  ]) {
    const reply = await inConsole(text);
    assert.strictEqual(reply.success, false, text);
    assert.match(reply.message, reason, text);
  }

  await vscode.commands.executeCommand("workbench.debug.action.focusWatchView");
  const watched = await onSelection(
    "editor.debug.action.selectionToWatch",
    "where->y + x",
    "watch",
  );
  assert.strictEqual(watched.body.result, "22");

  const invalidated = event("invalidated");
  const reread = answer(
    "evaluate",
    (arguments_) => arguments_.context === "watch" && arguments_.expression === "where->y + x",
  );
  assert.strictEqual((await inConsole("x = 7")).body.result, "7");
  await invalidated;
  assert.strictEqual((await reread).body.result, "9");

  const hover = answer(
    "evaluate",
    (arguments_) => arguments_.context === "hover" && arguments_.expression === "where->y",
  );
  const opened = await vscode.window.showTextDocument(editor.document);
  const lineText = opened.document.lineAt(line).text;
  const hovered = lineText.indexOf("where->y") + "where->".length;
  opened.selection = new vscode.Selection(line, hovered, line, hovered);
  await vscode.commands.executeCommand("editor.debug.action.showDebugHover");
  assert.strictEqual((await hover).body.result, "2");

  for (const [text, column, expression] of [
    ["total += items[i + 1].value;", 23, "items[i + 1].value"],
    ["return ns::config.limit;", 19, "ns::config.limit"],
    ["x = $rip;", 6, "$rip"],
    ["f(a)->b", 6, "f(a)->b"],
    ["(*node).next", 10, "(*node).next"],
    ["len(items).x", 11, "len(items).x"],
    ["// 12", 3, undefined],
    ["ab", 5, undefined],
    ["ab", -1, undefined],
  ]) {
    assert.strictEqual(extension().expressionAt(text, column)?.expression, expression, text);
  }

  const formatted = request("microscope/setValueFormat", (arguments_) => arguments_.hex === true);
  const hexadecimal = answer(
    "evaluate",
    (arguments_) => arguments_.context === "watch" && arguments_.expression === "where->y + x",
  );
  await vscode.commands.executeCommand("microscope.toggleHexadecimal");
  await formatted;
  assert.strictEqual((await hexadecimal).body.result, "0x9");
  const decimal = request("microscope/setValueFormat", (arguments_) => arguments_.hex === false);
  await vscode.commands.executeCommand("microscope.toggleHexadecimal");
  await decimal;

  const values = await extension().provideInlineValues(
    editor.document,
    new vscode.Range(0, 0, line + 1, 0),
    {
      frameId: frame.frameId,
      stoppedLocation: new vscode.Range(line, 0, line, 0),
    },
  );
  const inline = values.map((value) => `${value.range.start.line - line}: ${value.text}`);
  for (const expected of [
    "0: x = 7",
    "0: list = 5",
    "0: shadowed = -1",
    `${lineOf(file, "int x = n * 2") - line}: x = 7`,
  ]) {
    assert.ok(inline.includes(expected), `${expected} in ${inline}`);
  }

  assert.ok(!inline.some((value) => value.endsWith(": y = 2")), `${inline}`);

  assert.deepStrictEqual(extension().uses(["a"], "a", -1, 0), []);
  await vscode.commands.executeCommand("workbench.action.closeAllEditors");
  await vscode.commands.executeCommand("workbench.debug.viewlet.action.removeAllWatchExpressions");

  await finish(1);
}

async function completionsAndLocations() {
  const file = source("variables.c");
  vscode.debug.addBreakpoints([sourceBreakpoint(file, "return **pointer_pointer")]);
  await vscode.commands.executeCommand("workbench.debug.action.focusVariablesView");
  const listed = response("variables", (reply) =>
    reply.body?.variables?.some((variable) => variable.name === "function_pointer"),
  );
  const { session, frame } = await launchToStop(
    { name: "vscode-locations", program: fixture("variables-gcc-o0") },
    "breakpoint",
  );

  const pointer = (await listed).message.body.variables.find(
    (variable) => variable.name === "function_pointer",
  );
  assert.ok(pointer.valueLocationReference > 0, JSON.stringify(pointer));
  const location = await session.customRequest("locations", {
    locationReference: pointer.valueLocationReference,
  });
  assert.strictEqual(location.source.path, file);
  assert.strictEqual(location.line, lineOf(file, "static int pointer_identity(int value)") + 1);
  const complete = async (text) =>
    (
      await session.customRequest("completions", {
        text,
        column: text.length + 1,
        frameId: frame.frameId,
      })
    ).targets.map((target) => target.label);
  assert.deepStrictEqual(await complete("pair."), ["first", "second"]);
  assert.deepStrictEqual(await complete("structure_pointer->s"), ["second"]);
  assert.ok((await complete("$rs")).includes("rsp"));
  assert.ok((await complete("pointer_parameter_v")).includes("pointer_parameter_value"));
  await finish();
}

async function dataBreakpointsAndTheirModes() {
  const breakpoint = new vscode.FunctionBreakpoint("scalar_stores");
  vscode.debug.addBreakpoints([breakpoint]);
  const { session, frame } = await launchToStop(
    { name: "vscode-data", program: fixture("watch-gcc-o0") },
    "function breakpoint",
  );
  await remove(breakpoint);
  const capabilities = await session.customRequest("dataBreakpointInfo", {
    name: "watch_i32",
    frameId: frame.frameId,
  });
  assert.deepStrictEqual(capabilities.accessTypes, ["write", "readWrite"]);
  const set = await session.customRequest("setDataBreakpoints", {
    breakpoints: [{ dataId: capabilities.dataId, accessType: "write", mode: "store" }],
  });
  assert.strictEqual(set.breakpoints[0].verified, true);
  for (const description of [
    "watch_i32 changed from 0 to 1",
    "watch_i32 changed from 1 to 2",
    "watch_i32 was written; it is still 2",
    "watch_i32 changed from 2 to 42",
  ]) {
    const stopped = event("stopped", (body) => body.reason === "data breakpoint");
    await vscode.commands.executeCommand("workbench.action.debug.continue");
    assert.strictEqual((await stopped).message.body.description, description);
    await focus();
  }
  const removed = event(
    "breakpoint",
    (body) => body.reason === "removed" && body.breakpoint.id === set.breakpoints[0].id,
  );
  assert.match((await inConsole("unwatch all")).body.result, /1 watchpoint/u);
  await removed;

  await session.customRequest("setExceptionBreakpoints", { filters: [] });
  await finish();
}

async function dataBreakpointConditions() {
  const breakpoint = new vscode.FunctionBreakpoint("caller");
  vscode.debug.addBreakpoints([breakpoint]);
  const { session, frame } = await launchToStop(
    {
      name: "vscode-data-conditions",
      program: fixture("hit-counts-gcc-o0"),
    },
    "function breakpoint",
  );
  await remove(breakpoint);
  const { dataId } = await session.customRequest("dataBreakpointInfo", {
    name: "last_call",
    frameId: frame.frameId,
  });
  const set = async (options) =>
    (
      await session.customRequest("setDataBreakpoints", {
        breakpoints: [{ dataId, accessType: "write", ...options }],
      })
    ).breakpoints[0];

  const next = async (id) => {
    const stopped = event("stopped", (body) => body.reason === "data breakpoint");
    await vscode.commands.executeCommand("workbench.action.debug.continue");
    const { body } = (await stopped).message;
    assert.deepStrictEqual(body.hitBreakpointIds, [id]);
    const at = await focus();
    const call = await session.customRequest("evaluate", {
      expression: "call",
      frameId: at.frameId,
      context: "watch",
    });
    return [body.description, call.result];
  };

  const first = await set({ condition: "call % 10 == 0" });
  assert.strictEqual(first.verified, true);
  assert.deepStrictEqual(await next(first.id), ["last_call changed from 9 to 10", "10"]);

  const edited = await set({ condition: "call % 10 == 5", hitCondition: ">=12" });
  assert.deepStrictEqual([edited.id, edited.verified], [first.id, true]);
  assert.deepStrictEqual(await next(first.id), ["last_call changed from 14 to 15", "15"]);

  await set({ condition: "no_such_value > 1" });
  const explained = event("output", (body) => /could not be evaluated/u.test(body.output));
  assert.deepStrictEqual(await next(first.id), ["last_call changed from 15 to 16", "16"]);
  assert.strictEqual(
    (await explained).message.body.output,
    `data breakpoint ${first.id} stopped because its condition could not be evaluated: ` +
      "no variable is named `no_such_value` here\n",
  );

  const refused = await set({ condition: "call = 3" });
  assert.strictEqual(refused.verified, false);
  assert.match(refused.message, /cannot assign; compare with `==`/u);
  const bare = await set({ hitCondition: "5" });
  assert.strictEqual(bare.verified, false);
  assert.match(bare.message, /a bare count is ambiguous; write ==5 .* or >=5/u);
  await finish();
}

async function crashAtASignal() {
  const exception = answer("exceptionInfo");
  const { body } = await launchToStop(
    { name: "vscode-crash", program: fixture("crash-gcc-o0") },
    "exception",
  );
  assert.strictEqual(body.text, "SIGSEGV");

  const info = (await exception).body;
  assert.strictEqual(info.exceptionId, "SIGSEGV");
  const explained = event("output", (output) => /terminated by SIGSEGV/u.test(output.output));
  await finish(139);
  await explained;
}

async function goroutinesStepsPanicsAndStacks() {
  const breakpoint = new vscode.FunctionBreakpoint("main.explode");
  vscode.debug.addBreakpoints([breakpoint]);
  const { session, threadId } = await launchToStop(
    { name: "vscode-go", program: fixture("panic-go-o0") },
    "function breakpoint",
  );

  assert.strictEqual(threadId, 1);
  const { threads } = await session.customRequest("threads");
  const workers = threads.filter((thread) => / main\.worker — chan receive/u.test(thread.name));
  assert.strictEqual(workers.length, 5, JSON.stringify(threads));
  assert.ok(
    workers.some((thread) => /\{job: resize, tenant: "a b"\}/u.test(thread.name)),
    JSON.stringify(workers),
  );

  const stepped = event("stopped", (body) => body.reason === "step");
  await vscode.commands.executeCommand("workbench.action.debug.stepOver");
  assert.strictEqual((await stepped).message.body.threadId, threadId);
  await focus();
  assert.strictEqual((await topFrame(session, threadId)).name, "main.explode");

  const panicked = event("stopped", (body) => body.reason === "exception");
  await vscode.commands.executeCommand("workbench.action.debug.stepOver");
  assert.strictEqual((await panicked).message.body.threadId, threadId);
  const atPanic = await focus();
  const { stackFrames } = await session.customRequest("stackTrace", { threadId });
  const raised = stackFrames.find((frame) => frame.id === atPanic.frameId);
  assert.strictEqual(raised?.name, "main.explode", JSON.stringify(stackFrames));
  const exception = await session.customRequest("exceptionInfo", { threadId });
  assert.match(exception.description, /the workers are waiting/u);
  await finish(2);

  const switched = new vscode.FunctionBreakpoint("runtime.readmemstats_m");
  vscode.debug.addBreakpoints([switched]);
  const stacks = await launchToStop(
    { name: "vscode-go-stacks", program: fixture("stacks-go-o0") },
    "function breakpoint",
  );
  const trace = await stacks.session.customRequest("stackTrace", { threadId: stacks.threadId });
  const labels = trace.stackFrames
    .filter((frame) => frame.presentationHint === "label")
    .map((frame) => frame.name);
  assert.deepStrictEqual(
    labels.slice(0, 2),
    ["on the runtime's stack", "on the task's stack"],
    JSON.stringify(trace),
  );

  await remove(switched);
  const signalled = event(
    "stopped",
    (body) => body.reason === "exception" && body.text === "SIGUSR1",
  );
  await vscode.commands.executeCommand("workbench.action.debug.continue");
  assert.strictEqual((await signalled).message.body.threadId, stacks.threadId);
  await focus();
  await stop(stacks.session);
}

async function pythonBreakpointsStepsAndExceptions() {
  const script = path.join(root, "tests/fixtures/python/uat.py");
  for (const build of ["3.14", "3.12"]) {
    vscode.debug.addBreakpoints([sourceBreakpoint(script, "# AREA_LINE")]);
    const {
      session,
      threadId,
      frame: focusedAtLaunch,
    } = await launchToStop(
      {
        name: `vscode-python-${build}`,
        program: path.join(process.env.MICROSCOPE_CPYTHON, build, "python"),
        args: ["-P", "-s", "-B", script],
      },
      "breakpoint",
    );
    let frame = await shown(session, threadId, focusedAtLaunch);
    assert.strictEqual(frame?.name, "area", build);
    assert.strictEqual(frame.line, lineOf(script, "# AREA_LINE") + 1, build);

    const stepped = event("stopped", (body) => body.reason === "step");
    await vscode.commands.executeCommand("workbench.action.debug.stepOver");
    await stepped;
    frame = await shown(session, threadId, await focus());
    assert.strictEqual(frame?.name, "area", build);
    assert.strictEqual(frame.line, lineOf(script, "# AREA_LINE") + 2, build);

    await session.customRequest("setExceptionBreakpoints", { filters: ["python-raised"] });
    const raised = event("stopped", (body) => body.reason === "exception");
    await vscode.commands.executeCommand("workbench.action.debug.continue");
    await raised;
    frame = await shown(session, threadId, await focus());
    assert.strictEqual(frame?.name, "fail", build);
    const exception = await session.customRequest("exceptionInfo", { threadId });
    assert.match(exception.description, /the shape is wrong/u, build);
    await session.customRequest("setExceptionBreakpoints", { filters: [] });
    await finish(0);
  }
}

const NODE_BUILDS = ["26", "24", "22", "debian-24"];
const nodeOf = (build) => path.join(process.env.MICROSCOPE_NODE, build, "node");

async function withNodeAdapter(build, body, prefix) {
  if (!build.startsWith("debian-")) {
    return body();
  }
  const wrapper = path.join(workspace().uri.fsPath, "microscope-in-namespace");
  const enter = path.join(path.dirname(nodeOf(build)), "enter");
  fs.writeFileSync(wrapper, `#!/bin/sh\nexec ${prefix ?? `'${enter}'`} microscope "$@"\n`, {
    mode: 0o755,
  });
  const settings = vscode.workspace.getConfiguration("microscope");
  try {
    await settings.update("path", wrapper, vscode.ConfigurationTarget.Global);
    return await body();
  } finally {
    await settings.update("path", undefined, vscode.ConfigurationTarget.Global);
    fs.rmSync(wrapper, { force: true });
  }
}

async function shown(session, threadId, item) {
  const { stackFrames } = await session.customRequest("stackTrace", { threadId });
  return stackFrames.find((frame) => frame.id === item.frameId);
}

async function valueIn(session, frame, scope, name) {
  const { scopes } = await session.customRequest("scopes", { frameId: frame.id });
  const found = scopes.find((each) => each.name === scope);
  assert.ok(found, `no ${scope} in ${JSON.stringify(scopes)}`);
  const { variables } = await session.customRequest("variables", {
    variablesReference: found.variablesReference,
  });
  return variables.find((variable) => variable.name === name)?.value;
}

async function nodeBreakpointsStepsAndExceptions() {
  const script = path.join(root, "tests/fixtures/node/uat.js");
  for (const build of NODE_BUILDS)
    await withNodeAdapter(build, async () => {
      vscode.debug.addBreakpoints([sourceBreakpoint(script, "// AREA_LINE")]);
      const {
        session,
        threadId,
        frame: focusedAtLaunch,
      } = await launchToStop(
        {
          name: `vscode-node-${build}`,
          program: nodeOf(build),
          args: [script],
        },
        "breakpoint",
      );
      let frame = await shown(session, threadId, focusedAtLaunch);
      assert.strictEqual(frame?.name, "area", build);
      assert.strictEqual(frame.line, lineOf(script, "// AREA_LINE") + 1, build);
      assert.ok(frame.column > 1, `${build}: ${JSON.stringify(frame)}`);

      const stepped = event("stopped", (body) => body.reason === "step");
      await vscode.commands.executeCommand("workbench.action.debug.stepOver");
      await stepped;
      frame = await shown(session, threadId, await focus());
      assert.strictEqual(frame?.name, "area", build);
      assert.strictEqual(frame.line, lineOf(script, "// RETURN_LINE") + 1, build);

      assert.strictEqual(await valueIn(session, frame, "Locals", "result"), "16", build);
      assert.strictEqual(await valueIn(session, frame, "Arguments", "side"), "4", build);

      await session.customRequest("setExceptionBreakpoints", { filters: ["javascript-thrown"] });
      const thrown = event("stopped", (body) => body.reason === "exception");
      await vscode.commands.executeCommand("workbench.action.debug.continue");
      await thrown;
      frame = await shown(session, threadId, await focus());
      assert.strictEqual(frame?.name, "fail", build);
      assert.strictEqual(frame.line, lineOf(script, "// THROW_LINE") + 1, build);
      const exception = await session.customRequest("exceptionInfo", { threadId });
      assert.match(exception.description, /the shape is wrong/u, build);
      await session.customRequest("setExceptionBreakpoints", { filters: [] });
      await finish(0);
    });
}

async function nodeTypeScriptThroughSourceMaps() {
  const script = path.join(root, "tests/fixtures/node/maps/app.ts");

  const scratch = path.join(root, "target/uat-node-maps");
  fs.mkdirSync(scratch, { recursive: true });
  const bundle = path.join(scratch, "app.js");
  childProcess.execFileSync(process.env.MICROSCOPE_ESBUILD, [
    script,
    "--bundle",
    "--platform=node",
    "--sourcemap",
    "--log-level=warning",
    "--format=cjs",
    `--outfile=${bundle}`,
  ]);
  const compilers = [
    ["tsx", ["--import", path.join(process.env.MICROSCOPE_TSX, "dist/loader.mjs"), script]],
    ["esbuild", [bundle]],
  ];
  try {
    for (const build of NODE_BUILDS)
      await withNodeAdapter(build, async () => {
        for (const [compiler, args] of compilers) {
          const context = `${build} ${compiler}`;
          vscode.debug.addBreakpoints([sourceBreakpoint(script, "// NORM")]);
          const {
            session,
            threadId,
            frame: focusedAtLaunch,
          } = await launchToStop(
            {
              name: `vscode-node-${compiler}-${build}`,
              program: nodeOf(build),
              args,
            },
            "breakpoint",
          );
          let frame = await shown(session, threadId, focusedAtLaunch);
          assert.strictEqual(frame?.name, "norm", context);
          assert.strictEqual(frame.source?.path, script, context);
          assert.strictEqual(frame.line, lineOf(script, "// NORM") + 1, context);

          const stepped = event("stopped", (body) => body.reason === "step");
          await vscode.commands.executeCommand("workbench.action.debug.stepOver");
          await stepped;
          frame = await shown(session, threadId, await focus());
          assert.strictEqual(frame?.source?.path, script, context);
          assert.strictEqual(frame.line, lineOf(script, "// RETURN") + 1, context);
          assert.strictEqual(await valueIn(session, frame, "Locals", "squared"), "25", context);

          await session.customRequest("setExceptionBreakpoints", {
            filters: ["javascript-thrown"],
          });
          const thrown = event("stopped", (body) => body.reason === "exception");
          await vscode.commands.executeCommand("workbench.action.debug.continue");
          await thrown;
          frame = await shown(session, threadId, await focus());
          assert.strictEqual(frame?.name, "fail", context);
          assert.strictEqual(frame.source?.path, script, context);
          assert.strictEqual(frame.line, lineOf(script, "// THROW") + 1, context);
          const exception = await session.customRequest("exceptionInfo", { threadId });
          assert.match(exception.description, /mapped/u, context);
          await session.customRequest("setExceptionBreakpoints", { filters: [] });
          await finish(0);
        }
      });
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
}

async function nodeWorkerStopsOnItsThread() {
  const script = path.join(root, "tests/fixtures/node/workers.js");
  for (const build of NODE_BUILDS)
    await withNodeAdapter(build, async () => {
      vscode.debug.addBreakpoints([sourceBreakpoint(script, "// CRUNCH")]);
      const started = event("process");
      const {
        session,
        threadId,
        frame: focusedAtLaunch,
      } = await launchToStop(
        {
          name: `vscode-node-worker-${build}`,
          program: nodeOf(build),
          args: [script],
        },
        "breakpoint",
      );

      const main = (await started).message.body.systemProcessId;
      assert.notStrictEqual(threadId, main, build);
      const { threads } = await session.customRequest("threads");
      const worker = threads.find((thread) => thread.id === threadId);
      assert.ok(worker, `${build}: ${threadId} in ${JSON.stringify(threads)}`);
      let frame = await shown(session, threadId, focusedAtLaunch);
      assert.strictEqual(frame?.name, "crunch", build);
      assert.strictEqual(frame.line, lineOf(script, "// CRUNCH") + 1, build);
      assert.strictEqual(await valueIn(session, frame, "Arguments", "n"), "6", build);

      const stepped = event("stopped", (body) => body.reason === "step");
      await vscode.commands.executeCommand("workbench.action.debug.stepOver");
      assert.strictEqual((await stepped).message.body.threadId, threadId, build);
      frame = await shown(session, threadId, await focus());
      assert.strictEqual(frame?.line, lineOf(script, "// AFTER") + 1, build);
      assert.strictEqual(await valueIn(session, frame, "Locals", "squared"), "36", build);
      await finish(0);
    });
}

async function nodeAttachedByProcessId() {
  const script = path.join(root, "tests/fixtures/node/attach.js");
  for (const build of NODE_BUILDS) {
    const debian = build.startsWith("debian-");
    const command = debian
      ? [path.join(path.dirname(nodeOf(build)), "enter"), nodeOf(build), script]
      : [nodeOf(build), script];

    const target = childProcess.spawn(command[0], command.slice(1), {
      stdio: ["pipe", "pipe", "inherit"],
      env: {},
    });
    const exit = new Promise((resolve) => {
      target.once("exit", resolve);
    });
    try {
      await within(
        `node ${build} to be ready`,
        new Promise((resolve, reject) => {
          target.stdout.once("data", (data) =>
            String(data).startsWith("READY") ? resolve() : reject(new Error(`readiness ${data}`)),
          );
          target.once("error", reject);
        }),
      );
      const prefix = debian
        ? `'${process.env.MICROSCOPE_UAT_NSENTER}' --target ${target.pid} --user --mount --preserve-credentials`
        : undefined;
      await withNodeAdapter(
        build,
        async () => {
          const breakpoint = sourceBreakpoint(script, "// SERVED");
          const placed = answer(
            "setBreakpoints",
            (arguments_) => arguments_.source.path === script,
          );
          const confirmed = event("breakpoint", (body) => body.breakpoint.verified);
          confirmed.catch(() => {});
          const attached = event("process");
          assert.ok(
            await vscode.debug.startDebugging(undefined, {
              type: "microscope",
              request: "attach",
              name: `vscode-node-attach-${build}`,
              pid: String(target.pid),
            }),
          );
          const { session, message: announced } = await attached;
          assert.strictEqual(announced.body.systemProcessId, target.pid, build);
          vscode.debug.addBreakpoints([breakpoint]);

          if (!(await placed).body.breakpoints[0].verified) {
            await confirmed;
          }
          const line = lineOf(script, "// SERVED") + 1;
          let stopped = event("stopped", (body) => body.reason === "breakpoint");
          target.stdin.write("x");
          let { message: sent } = await stopped;
          assert.strictEqual(sent.body.threadId, target.pid, build);
          let frame = await shown(session, sent.body.threadId, await focus());
          assert.deepStrictEqual([frame?.name, frame?.line], ["served", line], build);
          assert.strictEqual(await valueIn(session, frame, "Arguments", "n"), "41", build);

          stopped = event("stopped", (body) => body.reason === "breakpoint");
          await vscode.commands.executeCommand("workbench.action.debug.continue");
          ({ message: sent } = await stopped);
          assert.notStrictEqual(sent.body.threadId, target.pid, build);
          frame = await shown(session, sent.body.threadId, await focus());
          assert.deepStrictEqual([frame?.name, frame?.line], ["served", line], build);
          assert.strictEqual(await valueIn(session, frame, "Arguments", "n"), "7", build);
          await finish(0);
        },
        prefix,
      );
      assert.strictEqual(await within(`node ${build} to exit`, exit), 0, build);
    } finally {
      target.kill();
    }
  }
}

async function pauseAndThreads() {
  const breakpoint = new vscode.FunctionBreakpoint("worker_breakpoint");
  vscode.debug.addBreakpoints([breakpoint]);
  const { session, threadId } = await launchToStop(
    { name: "vscode-threads", program: fixture("threads") },
    "function breakpoint",
  );

  const { threads } = await session.customRequest("threads");
  assert.strictEqual(threads.length, 3, JSON.stringify(threads));
  assert.ok(threads.some((thread) => thread.id === threadId));
  await finish();

  const spinning = event("thread", (body) => body.reason === "started").then(() =>
    answer("threads"),
  );
  assert.ok(
    await vscode.debug.startDebugging(undefined, {
      type: "microscope",
      request: "launch",
      name: "vscode-spin",
      program: fixture("spin"),
      cwd: root,
    }),
  );
  await spinning;
  const spin = vscode.debug.activeDebugSession;

  const paused = event("stopped", (body) => body.reason === "pause");
  await vscode.commands.executeCommand("workbench.action.debug.pause", { sessionId: spin.id });
  await paused;
  await focus();
  await stop(spin);
}

async function librariesAndTheirSources() {
  const file = source("shared/library.c");
  const breakpoint = sourceBreakpoint(file, "return *dso_pointer");
  const after = new vscode.FunctionBreakpoint("after_unload");
  vscode.debug.addBreakpoints([breakpoint, after]);
  const pending = answer("setBreakpoints", (arguments_) => arguments_.source.path === file);
  const { session } = await launchToStop(
    {
      name: "vscode-library",
      program: fixture("globals-shared"),
      stopOnEntry: true,
    },
    "entry",
  );
  assert.strictEqual((await pending).body.breakpoints[0].reason, "pending");

  const listed = answer("loadedSources");
  await vscode.commands.executeCommand("workbench.debug.loadedScriptsView.focus");
  assert.ok(!(await listed).body.sources.some((entry) => entry.path === file));
  const loaded = event(
    "loadedSource",
    (body) => body.reason === "new" && body.source.path === file,
  );
  const resolved = event(
    "breakpoint",
    (body) => body.breakpoint.verified && body.breakpoint.source?.path === file,
  );
  const hit = event("stopped", (body) => body.reason === "breakpoint");
  await vscode.commands.executeCommand("workbench.action.debug.continue");
  await loaded;
  await resolved;
  const { message: stopped } = await hit;
  await focus();
  assert.strictEqual((await topFrame(session, stopped.body.threadId)).source.path, file);
  const unloaded = event(
    "loadedSource",
    (body) => body.reason === "removed" && body.source.path === file,
  );
  const unloadStop = event("stopped", (body) => body.reason === "function breakpoint");
  await vscode.commands.executeCommand("workbench.action.debug.continue");
  await unloaded;
  await unloadStop;
  await focus();
  await stop(session);
  vscode.debug.removeBreakpoints(vscode.debug.breakpoints);
}

async function runInTheIntegratedTerminal() {
  const breakpoint = sourceBreakpoint(source("process-environment.c"), "if (fgets(line");
  vscode.debug.addBreakpoints([breakpoint]);
  const stopped = event("stopped", (body) => body.reason === "breakpoint");
  const process = event("process");
  assert.ok(
    await vscode.debug.startDebugging(undefined, {
      type: "microscope",
      request: "launch",
      name: "vscode-terminal",
      program: fixture("process-environment"),
      args: ["one"],
      console: "integratedTerminal",
      cwd: root,
    }),
  );
  await stopped;
  await focus();
  assert.strictEqual((await process).message.body.startMethod, "launch");
  const terminal = vscode.window.terminals.find((candidate) =>
    candidate.name.includes("process-environment"),
  );
  assert.ok(
    terminal,
    `a terminal among ${vscode.window.terminals.map((candidate) => candidate.name)}`,
  );
  await remove(breakpoint);
  const exited = event("exited");
  const ended = terminated();
  await vscode.commands.executeCommand("workbench.action.debug.continue");

  terminal.sendText("typed");
  assert.strictEqual((await exited).message.body.exitCode, 2);
  await ended;
  terminal.dispose();
}

async function attachTarget() {
  const target = childProcess.spawn(fixture("attach"), [], { stdio: ["pipe", "pipe", "inherit"] });
  const exit = new Promise((resolve) => {
    target.once("exit", resolve);
  });
  await new Promise((resolve, reject) => {
    target.stdout.once("data", (data) =>
      String(data).startsWith("READY") ? resolve() : reject(new Error(`readiness ${data}`)),
    );
    target.once("error", reject);
  });
  return { target, exit };
}

async function attachToAProcess() {
  const { target, exit } = await attachTarget();
  try {
    const process = event("process");
    assert.ok(
      await vscode.debug.startDebugging(undefined, {
        type: "microscope",
        request: "attach",
        name: "vscode-attach",
        pid: String(target.pid),
      }),
    );
    const { session } = await process;

    const breakpoint = new vscode.FunctionBreakpoint("attach_breakpoint");
    const placed = answer(
      "setFunctionBreakpoints",
      (arguments_) => arguments_.breakpoints.length === 1,
    );
    vscode.debug.addBreakpoints([breakpoint]);
    assert.strictEqual((await placed).body.breakpoints[0].verified, true);

    const detached = answer("disconnect", (arguments_) => arguments_.restart === true);
    const again = event("process", (body) => body.systemProcessId === target.pid);
    await vscode.commands.executeCommand("workbench.action.debug.restart");
    assert.ok((await detached).success);
    await again;
    const stopped = event("stopped", (body) => body.reason === "function breakpoint");
    target.stdin.write("x");
    const { message: sent } = await stopped;
    assert.strictEqual((await topFrame(session, sent.body.threadId)).name, "attach_breakpoint");
    await remove(breakpoint);

    const ended = terminated();
    await vscode.debug.stopDebugging(session);
    await ended;
    assert.strictEqual(await within("the process to exit", exit), 23);
  } finally {
    target.kill();
  }
}

async function pickAProcess() {
  const { target, exit } = await attachTarget();
  try {
    const { processes } = extension();
    const listed = processes();
    assert.deepStrictEqual(
      listed
        .filter((entry) => entry.pid === target.pid)
        .map(({ name, commandLine, executable }) => ({ name, commandLine, executable })),
      [{ name: "attach", commandLine: fixture("attach"), executable: fixture("attach") }],
    );
    assert.ok(
      !listed.some((entry) => entry.pid === process.pid),
      "the extension host is not listed",
    );

    const attached = event("process");
    const started = vscode.debug.startDebugging(undefined, {
      type: "microscope",
      request: "attach",
      name: "vscode-pick",
      program: fixture("attach"),
      pid: "${command:pickProcess}",
    });
    let settled = false;
    started.then(
      // oxlint-disable-next-line promise/always-return -- the callbacks only note that it settled
      () => {
        settled = true;
      },
      () => {
        settled = true;
      },
    );

    await until("the picker to be accepted", () => {
      if (!settled) {
        vscode.commands.executeCommand("workbench.action.acceptSelectedQuickOpenItem");
      }
      return settled;
    });
    assert.ok(await started);
    const { session, message: sent } = await attached;
    assert.strictEqual(sent.body.systemProcessId, target.pid);
    const ended = terminated();
    await vscode.debug.stopDebugging(session);
    await ended;
  } finally {
    target.kill();
    await exit;
  }
}

async function followAForkedChild() {
  const breakpoint = new vscode.FunctionBreakpoint("shared_work");
  vscode.debug.addBreakpoints([breakpoint]);
  const stopIn = (child) =>
    message(
      `the ${child ? "child" : "parent"} to stop`,
      (session, sent) =>
        sent.type === "event" &&
        sent.event === "stopped" &&
        sent.body.reason === "function breakpoint" &&
        (session.parentSession !== undefined) === child,
    );
  const parentStopped = stopIn(false);
  const childStopped = stopIn(true);
  const asked = message(
    "the adapter to ask for a child session",
    (_, sent) => sent.type === "request" && sent.command === "startDebugging",
  );

  const started = message(
    "VS Code to answer startDebugging",
    (_, sent) => sent.type === "response" && sent.command === "startDebugging" && sent.success,
    "client",
  );
  assert.ok(
    await vscode.debug.startDebugging(undefined, {
      type: "microscope",
      request: "launch",
      name: "vscode-fork",
      program: fixture("fork"),
      cwd: root,
      follow: { mode: "all", stop: "process" },
    }),
  );
  const asking = (await asked).message.arguments;
  assert.strictEqual(asking.request, "attach");
  const { session: parent, message: parentStop } = await parentStopped;
  const { session: child, message: childStop } = await childStopped;
  await started;
  assert.strictEqual(child.parentSession.id, parent.id);
  assert.strictEqual(child.configuration.pid, asking.configuration.pid);
  // oxlint-disable-next-line no-underscore-dangle -- the name microscope gives a child session's configuration
  assert.strictEqual(child.configuration.__microscopeMember.number, 2);
  assert.strictEqual(child.name, asking.configuration.name);
  for (const [session, stopped] of [
    [parent, parentStop],
    [child, childStop],
  ]) {
    assert.strictEqual(
      (await topFrame(session, stopped.body.threadId)).name,
      "shared_work",
      session.name,
    );
  }
  await remove(breakpoint);

  for (const [session, stopped] of [
    [child, childStop],
    [parent, parentStop],
  ]) {
    const exited = message(
      `${session.name} to exit`,
      (from, sent) => from.id === session.id && sent.type === "event" && sent.event === "exited",
    );
    const ended = terminated(session.name);
    await vscode.commands.executeCommand("workbench.action.debug.continue", undefined, {
      sessionId: session.id,
      threadId: stopped.body.threadId,
    });
    assert.strictEqual((await exited).message.body.exitCode, 0, session.name);
    await ended;
  }
}

async function openACoreDump() {
  const stopped = event("stopped", (body) => body.reason === "exception");
  const capabilities = event("capabilities");
  assert.ok(
    await vscode.debug.startDebugging(undefined, {
      type: "microscope",
      request: "attach",
      name: "vscode-core",
      coreFile: fixture("crash-gcc-o0-segv.core"),
      program: fixture("crash-gcc-o0"),
    }),
  );
  const { session, message: sent } = await stopped;
  assert.match(sent.body.text, /SIGSEGV/u);
  assert.strictEqual((await capabilities).message.body.capabilities.supportsRestartRequest, false);
  const trace = await session.customRequest("stackTrace", { threadId: sent.body.threadId });
  assert.ok(
    trace.stackFrames.some((frame) => frame.name === "main"),
    JSON.stringify(trace),
  );
  await focus();

  const disconnected = request("disconnect", (arguments_) => arguments_.restart === true);
  const reopened = event("stopped", (body) => body.reason === "exception");
  await vscode.commands.executeCommand("workbench.action.debug.restart");
  await disconnected;
  const { session: again } = await reopened;

  focused = undefined;
  await focus();
  const ended = terminated();
  await vscode.debug.stopDebugging(again);
  await ended;
}

async function refuseBadConfigurationsAndOfferPrograms() {
  for (const [name, configuration, reason] of [
    [
      "vscode-no-program",
      { request: "launch", program: fixture("no-such-program") },
      /failed to load .*no-such-program/u,
    ],
    [
      "vscode-bad-arguments",
      { request: "launch", program: fixture("basic"), args: "one" },
      /invalid launch configuration at args/u,
    ],
    ["vscode-no-process", { request: "attach", pid: 999999999 }, /process 999999999/u],
    ["vscode-no-core", { request: "attach", coreFile: fixture("no-such.core") }, /no-such\.core/u],
  ]) {
    const refused = response(configuration.request, (reply) => !reply.success);
    const disconnected = request("disconnect");
    const started = vscode.debug.startDebugging(undefined, {
      type: "microscope",
      name,
      ...configuration,
    });
    assert.match((await refused).message.message, reason, name);

    await assert.rejects(
      within("the refusal to be shown", started),
      (error) => /refused to show dialog/u.test(error.message) && reason.test(error.message),
    );
    await disconnected;
    assert.strictEqual(vscode.debug.activeDebugSession, undefined, name);
  }

  const folder = workspace().uri.fsPath;
  const build = path.join(folder, "build");
  fs.mkdirSync(path.join(build, "deps"), { recursive: true });
  try {
    fs.copyFileSync(fixture("basic"), path.join(build, "basic"));
    fs.chmodSync(path.join(build, "basic"), 0o755);

    fs.copyFileSync(fixture("libglobals.so"), path.join(build, "libglobals.so"));
    fs.chmodSync(path.join(build, "libglobals.so"), 0o755);
    fs.copyFileSync(fixture("basic"), path.join(build, "deps", "basic-test"));
    fs.chmodSync(path.join(build, "deps", "basic-test"), 0o755);
    fs.writeFileSync(path.join(build, "script"), "#!/bin/sh\n", { mode: 0o755 });
    const offered = extension().provideDebugConfigurations(workspace());
    assert.deepStrictEqual(
      offered.map((configuration) => configuration.program),
      ["${workspaceFolder}/build/basic"],
    );
    const exited = event("exited");
    const ended = terminated(offered[0].name);
    assert.ok(await vscode.debug.startDebugging(workspace(), offered[0]));
    assert.strictEqual((await exited).message.body.exitCode, 0);
    await ended;
  } finally {
    fs.rmSync(build, { recursive: true, force: true });
  }
}

exports.run = async function run() {
  const only = process.env.MICROSCOPE_UAT_ONLY?.split(",");
  try {
    for (const scenario of [
      createALaunchJson,
      findTheAdapterFromTheSetting,
      logTheProtocolToAFile,
      launchStepInspectAndRestart,
      stepIntoByInstructionAndRunToCursor,
      conditionsHitCountsLogpointsAndRefusals,
      consoleWatchHoverHexadecimalAndInlineValues,
      completionsAndLocations,
      dataBreakpointsAndTheirModes,
      dataBreakpointConditions,
      crashAtASignal,
      goroutinesStepsPanicsAndStacks,
      pythonBreakpointsStepsAndExceptions,
      nodeBreakpointsStepsAndExceptions,
      nodeTypeScriptThroughSourceMaps,
      nodeWorkerStopsOnItsThread,
      nodeAttachedByProcessId,
      pauseAndThreads,
      librariesAndTheirSources,
      runInTheIntegratedTerminal,
      attachToAProcess,
      pickAProcess,
      followAForkedChild,
      openACoreDump,
      refuseBadConfigurationsAndOfferPrograms,
    ].filter((candidate) => only === undefined || only.includes(candidate.name))) {
      console.log(`uat: ${scenario.name}`);
      await scenario();
      console.log(`uat: ${scenario.name} passed`);
    }
  } finally {
    save();
  }
};
