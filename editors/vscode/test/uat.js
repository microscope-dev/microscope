// User acceptance test: VS Code itself drives `microscope dap` through the
// extension, as a user would from the editor, and every message each
// session exchanges is recorded in the adapter log's format.
//
// Each scenario drives VS Code's own commands where VS Code has one for
// what a user does: breakpoints, stepping, the debug console, the watch
// view, hovers, run to cursor, the disassembly view, restart. It falls
// back on a session's requests only for what has no command, and checks
// what VS Code asks the adapter and what it is answered, which is what a
// user sees.
//
// Run by run.sh through VS Code's extension test mode.

'use strict';

const assert = require('assert');
const childProcess = require('child_process');
const fs = require('fs');
const path = require('path');
const vscode = require('vscode');

const root = process.env.MICROSCOPE_UAT_ROOT;
const record = process.env.MICROSCOPE_UAT_RECORD;
const fixture = (name) => path.join(root, 'build/test-programs', name);
const source = (name) => path.join(root, 'tests/fixtures/c', name);
const extension = () => require(path.join(root, 'editors/vscode/extension.js'));
const TIMEOUT = 15000;

// Every message of every session, and the waiters for adapter messages.
const transcripts = new Map();
let waiters = [];

vscode.debug.registerDebugAdapterTrackerFactory('microscope', {
    createDebugAdapterTracker(session) {
        const lines = [];
        // A restart that starts another adapter for the session keeps both
        // transcripts. A child session is named after its parent, since its
        // own name holds a process id.
        let name = session.parentSession === undefined
            ? session.configuration.name
            : `${session.parentSession.configuration.name}.child`;
        while (transcripts.has(name)) {
            name = `${name}.restarted`;
        }
        transcripts.set(name, lines);
        const notify = (direction, message) => {
            waiters = waiters.filter((waiter) => !waiter(session, message, direction));
        };
        return {
            onWillReceiveMessage: (message) => {
                lines.push(`<- ${JSON.stringify(message)}`);
                notify('client', message);
            },
            onDidSendMessage: (message) => {
                lines.push(`-> ${JSON.stringify(message)}`);
                notify('adapter', message);
            },
            onError: (error) => lines.push(`!! ${error}`),
        };
    },
});

/** Waits for a message the predicate accepts, from the adapter or, with `from`, the client. */
function message(description, predicate, from = 'adapter') {
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
    message(`a ${name} event`, (_, sent) =>
        sent.type === 'event' && sent.event === name && predicate(sent.body ?? {}));

const response = (command, predicate = () => true) =>
    message(`a ${command} response`, (_, sent) =>
        sent.type === 'response' && sent.command === command && predicate(sent));

/** Waits for a request VS Code sends the adapter. */
const request = (command, predicate = () => true) =>
    message(`VS Code to send ${command}`, (_, sent) =>
        sent.type === 'request' && sent.command === command && predicate(sent.arguments ?? {}), 'client');

/** Waits for the response to the next request VS Code sends that the predicate accepts. */
async function answer(command, predicate = () => true) {
    let seq;
    const sent = request(command, (arguments_) => predicate(arguments_));
    const answered = message(`the answer to ${command}`, (_, reply) =>
        reply.type === 'response' && reply.command === command && seq !== undefined && reply.request_seq === seq);
    seq = (await sent).message.seq;
    return (await answered).message;
}

/** Bounds a wait. */
function within(description, promise) {
    let timer;
    const timeout = new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`timed out waiting for ${description}`)), TIMEOUT);
    });
    return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/** Polls until the predicate holds, or the wait times out. */
function until(description, predicate) {
    return within(description, new Promise((resolve) => {
        const poll = () => (predicate() ? resolve() : setTimeout(poll, 20));
        poll();
    }));
}

/** Waits for a session to end: the one with the name, if given. */
function terminated(name) {
    return within('the session to end', new Promise((resolve) => {
        const listener = vscode.debug.onDidTerminateDebugSession((session) => {
            if (name === undefined || session.name === name) {
                listener.dispose();
                resolve(session);
            }
        });
    }));
}

/** Saves each session's transcript, named after its configuration. */
function save() {
    fs.mkdirSync(record, { recursive: true });
    for (const [name, lines] of transcripts) {
        const file = name.replace(/[^A-Za-z0-9._-]+/g, '-');
        fs.writeFileSync(path.join(record, `${file}.log`), `${lines.join('\n')}\n`);
    }
}

/** Removes breakpoints and waits until the adapter has them. */
async function remove(breakpoint) {
    const command = breakpoint instanceof vscode.FunctionBreakpoint ? 'setFunctionBreakpoints' : 'setBreakpoints';
    const applied = response(command);
    vscode.debug.removeBreakpoints([breakpoint]);
    await applied;
}

let focused;

/**
 * Waits for VS Code to focus a frame of a new stop, after which its
 * commands act on that stop. A session never reuses a frame id.
 */
function focus() {
    return new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('timed out waiting for a focused frame')), TIMEOUT);
        const check = (item) => {
            if (!(item instanceof vscode.DebugStackFrame) || `${item.session.id} ${item.frameId}` === focused) {
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
    const index = fs.readFileSync(file, 'utf8').split('\n').findIndex((line) => line.includes(marker));
    assert.notStrictEqual(index, -1, `${marker} in ${file}`);
    return index;
}

function sourceBreakpoint(file, marker, options = {}) {
    const location = new vscode.Location(vscode.Uri.file(file), new vscode.Position(lineOf(file, marker), 0));
    return new vscode.SourceBreakpoint(location, true, options.condition, options.hitCondition, options.logMessage);
}

async function topFrame(session, threadId) {
    const trace = await session.customRequest('stackTrace', { threadId, startFrame: 0, levels: 1 });
    return trace.stackFrames[0];
}

const workspace = () => vscode.workspace.workspaceFolders[0];

/** Starts a session and waits for its first stop. */
async function launchToStop(configuration, reason) {
    const stopped = event('stopped', (body) => reason === undefined || body.reason === reason);
    assert.ok(await vscode.debug.startDebugging(undefined, { type: 'microscope', request: 'launch', cwd: root, ...configuration }));
    const { session, message: sent } = await stopped;
    // VS Code focuses a frame with source, or else the stopped thread.
    const frame = reason === 'entry' ? await focusThread(sent.body.threadId) : await focus();
    return { session, threadId: sent.body.threadId, frame, body: sent.body };
}

/** Waits for VS Code to focus a thread, or a frame of it. */
function focusThread(threadId) {
    return until('a focused thread', () => {
        const item = vscode.debug.activeStackItem;
        return (item instanceof vscode.DebugThread || item instanceof vscode.DebugStackFrame)
            && item.threadId === threadId;
    });
}

/** Continues the focused session, removes every breakpoint first, and waits for it to end. */
async function finish(exitCode = 0) {
    for (const breakpoint of vscode.debug.breakpoints) {
        await remove(breakpoint);
    }
    const exited = event('exited');
    const ended = terminated();
    await vscode.commands.executeCommand('workbench.action.debug.continue');
    assert.strictEqual((await exited).message.body.exitCode, exitCode);
    await ended;
}

/** Ends a session by disconnecting from it. */
async function stop(session) {
    const ended = terminated(session.name);
    await vscode.debug.stopDebugging(session);
    await ended;
}

/**
 * Runs a VS Code command on text selected in a scratch editor, as a user
 * selecting text and choosing "Evaluate in Debug Console" or "Add to Watch"
 * does, and returns the adapter's answer to the evaluation it causes.
 */
async function onSelection(command, text, context) {
    const document = await vscode.workspace.openTextDocument({ content: text });
    const editor = await vscode.window.showTextDocument(document, { preview: true });
    editor.selection = new vscode.Selection(0, 0, 0, text.length);
    const answered = answer('evaluate', (arguments_) => arguments_.expression === text && arguments_.context === context);
    await vscode.commands.executeCommand(command);
    const result = await answered;
    await vscode.commands.executeCommand('workbench.action.revertAndCloseActiveEditor');
    return result;
}

/**
 * Evaluates a line in the debug console. Commands' output is styled for
 * VS Code, which shows ANSI styling; the result is returned without it.
 */
async function inConsole(text) {
    const reply = await onSelection('editor.debug.action.selectionToRepl', text, 'repl');
    // eslint-disable-next-line no-control-regex
    const plain = (styled) => styled?.replace(/\x1b\[[0-9;]*m/g, '');
    if (reply.body?.result !== undefined) {
        reply.body.result = plain(reply.body.result);
    }
    reply.message = plain(reply.message);
    return reply;
}

/** Pressing F5 in a folder without a launch.json creates one to fill in. */
async function createALaunchJson() {
    const folder = workspace().uri.fsPath;
    const program = path.join(folder, 'main.c');
    const launchJson = path.join(folder, '.vscode', 'launch.json');
    fs.writeFileSync(program, 'int main(void) { return 0; }\n');
    try {
        // The C file makes microscope the only debugger to guess.
        await vscode.window.showTextDocument(vscode.Uri.file(program));
        await vscode.commands.executeCommand('workbench.action.debug.start');
        await until('launch.json', () => fs.existsSync(launchJson));
        // VS Code writes its own comments at the top of the file.
        const text = fs.readFileSync(launchJson, 'utf8').replace(/^\s*\/\/.*$/gm, '');
        const { configurations } = JSON.parse(text);
        assert.deepStrictEqual(configurations.map(({ type, request: kind }) => ({ type, request: kind })), [
            { type: 'microscope', request: 'launch' },
        ]);
        assert.strictEqual(vscode.debug.activeDebugSession, undefined);
    } finally {
        await vscode.commands.executeCommand('workbench.action.closeAllEditors');
        fs.rmSync(path.join(folder, '.vscode'), { recursive: true, force: true });
        fs.rmSync(program, { force: true });
    }
}

/** The microscope.path setting chooses the adapter, and a missing one is named. */
async function findTheAdapterFromTheSetting() {
    const folder = workspace();
    const wrapper = path.join(folder.uri.fsPath, 'microscope-wrapper');
    const marker = `${wrapper}.used`;
    fs.writeFileSync(wrapper, `#!/bin/sh\ntouch '${marker}'\nexec microscope "$@"\n`, { mode: 0o755 });
    const settings = vscode.workspace.getConfiguration('microscope');
    const configuration = (name) => ({ type: 'microscope', request: 'launch', name, program: fixture('basic'), cwd: root });
    try {
        await settings.update('path', '${workspaceFolder}/missing-microscope', vscode.ConfigurationTarget.Global);
        await assert.rejects(
            vscode.debug.startDebugging(folder, configuration('vscode-missing-adapter')),
            /Cannot find microscope at "\$\{workspaceFolder\}\/missing-microscope"/,
        );

        await settings.update('path', '${workspaceFolder}/microscope-wrapper', vscode.ConfigurationTarget.Global);
        const exited = event('exited');
        const ended = terminated('vscode-setting');
        assert.ok(await vscode.debug.startDebugging(folder, configuration('vscode-setting')));
        assert.strictEqual((await exited).message.body.exitCode, 0);
        await ended;
        assert.ok(fs.existsSync(marker), 'the session ran the configured adapter');
    } finally {
        await settings.update('path', undefined, vscode.ConfigurationTarget.Global);
        fs.rmSync(wrapper, { force: true });
        fs.rmSync(marker, { force: true });
    }
}

/** The microscope.logFile setting has the adapter log every message. */
async function logTheProtocolToAFile() {
    const log = path.join(workspace().uri.fsPath, 'microscope-dap.log');
    const settings = vscode.workspace.getConfiguration('microscope');
    try {
        await settings.update('logFile', '${workspaceFolder}/microscope-dap.log', vscode.ConfigurationTarget.Global);
        const exited = event('exited');
        const ended = terminated('vscode-log');
        assert.ok(await vscode.debug.startDebugging(workspace(), {
            type: 'microscope', request: 'launch', name: 'vscode-log', program: fixture('basic'),
        }));
        await exited;
        await ended;
        const text = fs.readFileSync(log, 'utf8');
        assert.match(text, /"command":"initialize"/);
        assert.match(text, /"event":"exited"/);
    } finally {
        await settings.update('logFile', undefined, vscode.ConfigurationTarget.Global);
        fs.rmSync(log, { force: true });
    }
}

async function launchStepInspectAndRestart() {
    const breakpoint = sourceBreakpoint(source('basic.c'), 'return microscope_value;');
    vscode.debug.addBreakpoints([breakpoint]);
    const stopped = event('stopped', (body) => body.reason === 'breakpoint');
    assert.ok(await vscode.debug.startDebugging(undefined, {
        type: 'microscope', request: 'launch', name: 'vscode-launch', program: fixture('basic'), cwd: root,
    }));
    const { session, message: sent } = await stopped;
    const threadId = sent.body.threadId;
    // VS Code focuses the stopped frame itself.
    await focus();
    let frame = await topFrame(session, threadId);
    assert.strictEqual(frame.name, 'breakpoint_target');
    assert.strictEqual(frame.line, lineOf(source('basic.c'), 'return microscope_value;') + 1);
    const value = await session.customRequest('evaluate', {
        expression: 'microscope_value', frameId: frame.id, context: 'watch',
    });
    assert.strictEqual(value.result, '1234605616436508552');

    // Step out with the toolbar's command, then over with the keyboard's.
    let step = event('stopped', (body) => body.reason === 'step');
    await vscode.commands.executeCommand('workbench.action.debug.stepOut');
    await step;
    await focus();
    frame = await topFrame(session, threadId);
    assert.strictEqual(frame.name, 'main');
    step = event('stopped', (body) => body.reason === 'step');
    await vscode.commands.executeCommand('workbench.action.debug.stepOver');
    await step;
    await focus();

    // The disassembly view loads instructions around the current one.
    const disassembled = response('disassemble', (reply) => reply.success && reply.body.instructions.length > 0);
    await vscode.commands.executeCommand('debug.action.openDisassemblyView');
    await disassembled;

    // Restarting runs the program again to its breakpoint.
    const again = event('stopped', (body) => body.reason === 'breakpoint');
    await vscode.commands.executeCommand('workbench.action.debug.restart');
    await again;
    await focus();

    await remove(breakpoint);
    const exited = event('exited');
    const ended = terminated();
    await vscode.commands.executeCommand('workbench.action.debug.continue');
    assert.strictEqual((await exited).message.body.exitCode, 0);
    await ended;
}

/**
 * Stepping into a call, and with the disassembly view focused, by
 * instruction; run to cursor stops where the cursor is.
 */
async function stepIntoByInstructionAndRunToCursor() {
    const file = source('basic.c');
    const { session, threadId } = await launchToStop({
        name: 'vscode-stepping', program: fixture('basic'), stopOnEntry: true,
    }, 'entry');
    const editor = await vscode.window.showTextDocument(vscode.Uri.file(file));
    const line = lineOf(file, 'uint64_t second = breakpoint_target();');
    editor.selection = new vscode.Selection(line, 4, line, 4);
    let stopped = event('stopped');
    await vscode.commands.executeCommand('editor.debug.action.runToCursor');
    await stopped;
    await focus();
    assert.strictEqual((await topFrame(session, threadId)).line, line + 1);

    // Stepping in enters the call on the line.
    const into = request('stepIn');
    stopped = event('stopped', (body) => body.reason === 'step');
    await vscode.commands.executeCommand('workbench.action.debug.stepInto');
    assert.notStrictEqual((await into).message.arguments.granularity, 'instruction');
    await stopped;
    await focus();
    assert.strictEqual((await topFrame(session, threadId)).name, 'breakpoint_target');

    // With the disassembly view focused, steps are by instruction.
    const disassembled = response('disassemble', (reply) => reply.success);
    await vscode.commands.executeCommand('debug.action.openDisassemblyView');
    await disassembled;
    const before = (await topFrame(session, threadId)).instructionPointerReference;
    const next = request('next');
    stopped = event('stopped', (body) => body.reason === 'step');
    await vscode.commands.executeCommand('workbench.action.debug.stepOver');
    assert.strictEqual((await next).message.arguments.granularity, 'instruction');
    await stopped;
    await focus();
    const after = await topFrame(session, threadId);
    assert.notStrictEqual(after.instructionPointerReference, before);
    assert.strictEqual(after.name, 'breakpoint_target');
    await vscode.commands.executeCommand('workbench.action.closeAllEditors');
    await finish();
}

/**
 * Conditions, hit counts, and logpoints stop and print where they say,
 * and breakpoints that cannot be placed say why.
 */
async function conditionsHitCountsLogpointsAndRefusals() {
    const file = source('hit-counts.c');
    const breakpoints = [
        sourceBreakpoint(file, 'last_call = call;', { condition: 'call == 5' }),
        sourceBreakpoint(file, 'counted(call);', { logMessage: 'calling {call} after {last_call}' }),
        new vscode.FunctionBreakpoint('caller', true, undefined, '>= 39'),
        // Each of these says why it is not placed.
        sourceBreakpoint(file, 'shared(call);', { condition: 'call ==' }),
        sourceBreakpoint(file, 'shared(call + SECOND_SITE_OFFSET);', { condition: 'call = 3' }),
        sourceBreakpoint(file, 'for (uint64_t call = 1', { hitCondition: '5' }),
        new vscode.FunctionBreakpoint('no_such_function'),
    ];
    vscode.debug.addBreakpoints(breakpoints);
    const placed = answer('setBreakpoints', (arguments_) => arguments_.source.path === file);
    const functions = answer('setFunctionBreakpoints');
    const logged = event('output', (body) => body.output === 'calling 5 after 4\n');
    const { session, threadId, frame } = await launchToStop({
        name: 'vscode-breakpoints', program: fixture('hit-counts-gcc-o0'),
    }, 'breakpoint');
    const lines = (await placed).body.breakpoints;
    assert.deepStrictEqual(lines.slice(0, 2).map((breakpoint) => breakpoint.verified), [true, true]);
    for (const [breakpoint, reason] of [
        [lines[2], /invalid condition: expected an operand/],
        [lines[3], /assign/],
        [lines[4], /a bare count is ambiguous; write ==5 .* or >=5/],
    ]) {
        assert.strictEqual(breakpoint.verified, false, JSON.stringify(breakpoint));
        assert.match(breakpoint.message, reason);
    }
    const [caller, missing] = (await functions).body.breakpoints;
    assert.strictEqual(caller.verified, true);
    assert.strictEqual(missing.verified, false);
    assert.match(missing.message, /no_such_function/);

    // The condition held at the fifth call, after logging each before it.
    await logged;
    const call = await session.customRequest('evaluate', { expression: 'call', frameId: frame.frameId, context: 'watch' });
    assert.strictEqual(call.result, '5');
    await remove(breakpoints[0]);
    await remove(breakpoints[1]);
    // The function breakpoint stops from its 39th hit on.
    const hit = event('stopped', (body) => body.reason === 'function breakpoint');
    await vscode.commands.executeCommand('workbench.action.debug.continue');
    await hit;
    const at = await focus();
    const count = await session.customRequest('evaluate', { expression: 'call', frameId: at.frameId, context: 'watch' });
    assert.strictEqual(count.result, '39');
    assert.strictEqual((await topFrame(session, threadId)).name, 'caller');
    await finish();
}

/**
 * The debug console evaluates expressions, assigns, and runs commands; the
 * watch view and hovers show whole expressions; the variables view
 * follows assignments and the hexadecimal setting; values show inline.
 */
async function consoleWatchHoverHexadecimalAndInlineValues() {
    const file = source('command-names.c');
    const line = lineOf(file, 'volatile int sink');
    vscode.debug.addBreakpoints([sourceBreakpoint(file, 'volatile int sink')]);
    await vscode.commands.executeCommand('workbench.view.debug');
    // VS Code shows values inline by asking the extension, which asks the
    // adapter where each variable is declared.
    const declared = request('locations');
    const { session, frame } = await launchToStop({ name: 'vscode-console', program: fixture('command-names') }, 'breakpoint');
    const editor = await vscode.window.showTextDocument(vscode.Uri.file(file), { preserveFocus: false });
    await declared;

    // Lines are expressions unless they name a command the frame has no
    // variable for.
    for (const [text, result] of [
        ['x + 1', '21'], ['list', '5'], ['n', '10'], ['p/x list', '(int) list = 0x5'],
        ['(u8)-1', '255'], ['$rip == $pc', 'true'], ['sizeof(origin)', '8'],
    ]) {
        const reply = await inConsole(text);
        assert.ok(reply.success, JSON.stringify(reply));
        assert.strictEqual(reply.body.result, result, text);
    }
    assert.match((await inConsole('bt')).body.result, /^#0 .*names/);
    assert.match((await inConsole('whatis where')).body.result, /point \*/);
    // Mistakes are answered, not fatal.
    for (const [text, reason] of [
        ['next', /`next` is not available in the debug console/],
        ['x + missing', /missing[\s\S]*\^{7}/],
        ['origin + 1', /type/],
        ['1 / 0', /divi/],
    ]) {
        const reply = await inConsole(text);
        assert.strictEqual(reply.success, false, text);
        assert.match(reply.message, reason, text);
    }

    // Watching follows the stop.
    await vscode.commands.executeCommand('workbench.debug.action.focusWatchView');
    const watched = await onSelection('editor.debug.action.selectionToWatch', 'where->y + x', 'watch');
    assert.strictEqual(watched.body.result, '22');

    // Assigning in the console updates what the views show.
    const invalidated = event('invalidated');
    const reread = answer('evaluate', (arguments_) => arguments_.context === 'watch' && arguments_.expression === 'where->y + x');
    assert.strictEqual((await inConsole('x = 7')).body.result, '7');
    await invalidated;
    assert.strictEqual((await reread).body.result, '9');

    // Hovering a member shows the whole expression it ends. The mouse may
    // hover something else meanwhile.
    const hover = answer('evaluate', (arguments_) => arguments_.context === 'hover' && arguments_.expression === 'where->y');
    const shown = await vscode.window.showTextDocument(editor.document);
    const text = shown.document.lineAt(line).text;
    const column = text.indexOf('where->y') + 'where->'.length;
    shown.selection = new vscode.Selection(line, column, line, column);
    await vscode.commands.executeCommand('editor.debug.action.showDebugHover');
    assert.strictEqual((await hover).body.result, '2');
    // So does hovering an indexed or qualified name, which VS Code alone
    // would cut at its brackets or colons.
    for (const [text, column, expression] of [
        ['total += items[i + 1].value;', 23, 'items[i + 1].value'],
        ['return ns::config.limit;', 19, 'ns::config.limit'],
        ['x = $rip;', 6, '$rip'],
        ['f(a)->b', 6, 'f(a)->b'],
        ['(*node).next', 10, '(*node).next'],
        ['len(items).x', 11, 'len(items).x'],
        ['// 12', 3, undefined],
    ]) {
        assert.strictEqual(extension().expressionAt(text, column)?.expression, expression, text);
    }

    // The hexadecimal setting shows every value in hexadecimal.
    const formatted = request('microscope/setValueFormat', (arguments_) => arguments_.hex === true);
    const hexadecimal = answer('evaluate', (arguments_) => arguments_.context === 'watch' && arguments_.expression === 'where->y + x');
    await vscode.commands.executeCommand('microscope.toggleHexadecimal');
    await formatted;
    assert.strictEqual((await hexadecimal).body.result, '0x9');
    const decimal = request('microscope/setValueFormat', (arguments_) => arguments_.hex === false);
    await vscode.commands.executeCommand('microscope.toggleHexadecimal');
    await decimal;

    // Inline values are each variable's, on the lines it is in scope.
    const values = await extension().provideInlineValues(editor.document, new vscode.Range(0, 0, line + 1, 0), {
        frameId: frame.frameId, stoppedLocation: new vscode.Range(line, 0, line, 0),
    });
    const inline = values.map((value) => `${value.range.start.line - line}: ${value.text}`);
    for (const expected of ['0: x = 7', '0: list = 5', '0: shadowed = -1', `${lineOf(file, 'int x = n * 2') - line}: x = 7`]) {
        assert.ok(inline.includes(expected), `${expected} in ${inline}`);
    }
    // A member named like a variable is no use of it.
    assert.ok(!inline.some((value) => value.endsWith(': y = 2')), `${inline}`);
    await vscode.commands.executeCommand('workbench.action.closeAllEditors');
    await vscode.commands.executeCommand('workbench.debug.viewlet.action.removeAllWatchExpressions');
    // The program checks its sum, which the assignment to x changed.
    await finish(1);
}

/**
 * The variables view links a function pointer to its function, and the
 * console completes members, registers, and globals.
 */
async function completionsAndLocations() {
    const file = source('variables.c');
    vscode.debug.addBreakpoints([sourceBreakpoint(file, 'return **pointer_pointer')]);
    await vscode.commands.executeCommand('workbench.debug.action.focusVariablesView');
    const listed = response('variables', (reply) => reply.body?.variables?.some((variable) => variable.name === 'function_pointer'));
    const { session, frame } = await launchToStop({ name: 'vscode-locations', program: fixture('variables-gcc-o0') }, 'breakpoint');
    // VS Code shows the locals, where the function pointer's value links to
    // the function.
    const pointer = (await listed).message.body.variables.find((variable) => variable.name === 'function_pointer');
    assert.ok(pointer.valueLocationReference > 0, JSON.stringify(pointer));
    const location = await session.customRequest('locations', { locationReference: pointer.valueLocationReference });
    assert.strictEqual(location.source.path, file);
    assert.strictEqual(location.line, lineOf(file, 'static int pointer_identity(int value)') + 1);
    const complete = async (text) => (await session.customRequest('completions', {
        text, column: text.length + 1, frameId: frame.frameId,
    })).targets.map((target) => target.label);
    assert.deepStrictEqual(await complete('pair.'), ['first', 'second']);
    assert.deepStrictEqual(await complete('structure_pointer->s'), ['second']);
    assert.ok((await complete('$rs')).includes('rsp'));
    assert.ok((await complete('pointer_parameter_v')).includes('pointer_parameter_value'));
    await finish();
}

/**
 * Data breakpoints stop where the value changes, or at every store in
 * their store mode, and the console removing one removes the client's.
 */
async function dataBreakpointsAndTheirModes() {
    const breakpoint = new vscode.FunctionBreakpoint('scalar_stores');
    vscode.debug.addBreakpoints([breakpoint]);
    const { session, frame } = await launchToStop({ name: 'vscode-data', program: fixture('watch-gcc-o0') }, 'function breakpoint');
    await remove(breakpoint);
    const capabilities = await session.customRequest('dataBreakpointInfo', { name: 'watch_i32', frameId: frame.frameId });
    assert.deepStrictEqual(capabilities.accessTypes, ['write', 'readWrite']);
    const set = await session.customRequest('setDataBreakpoints', {
        breakpoints: [{ dataId: capabilities.dataId, accessType: 'write', mode: 'store' }],
    });
    assert.strictEqual(set.breakpoints[0].verified, true);
    for (const description of [
        'watch_i32 changed from 0 to 1', 'watch_i32 changed from 1 to 2',
        'watch_i32 was written; it is still 2', 'watch_i32 changed from 2 to 42',
    ]) {
        const stopped = event('stopped', (body) => body.reason === 'data breakpoint');
        await vscode.commands.executeCommand('workbench.action.debug.continue');
        assert.strictEqual((await stopped).message.body.description, description);
        await focus();
    }
    const removed = event('breakpoint', (body) => body.reason === 'removed' && body.breakpoint.id === set.breakpoints[0].id);
    assert.match((await inConsole('unwatch all')).body.result, /1 watchpoint/);
    await removed;
    // The program raises signals of its own later.
    await session.customRequest('setExceptionBreakpoints', { filters: [] });
    await finish();
}

/**
 * A data breakpoint's condition and hit count choose which changes stop,
 * and editing them keeps the data breakpoint and its count. VS Code edits
 * them only from its breakpoint widget, so the scenario sends the requests
 * VS Code sends after a "Break on Value Change" and each edit.
 */
async function dataBreakpointConditions() {
    const breakpoint = new vscode.FunctionBreakpoint('caller');
    vscode.debug.addBreakpoints([breakpoint]);
    const { session, frame } = await launchToStop({
        name: 'vscode-data-conditions', program: fixture('hit-counts-gcc-o0'),
    }, 'function breakpoint');
    await remove(breakpoint);
    const { dataId } = await session.customRequest('dataBreakpointInfo', { name: 'last_call', frameId: frame.frameId });
    const set = async (options) => (await session.customRequest('setDataBreakpoints', {
        breakpoints: [{ dataId, accessType: 'write', ...options }],
    })).breakpoints[0];
    // Continues to the next change, and returns how VS Code describes it
    // and the call the focused frame made.
    const next = async (id) => {
        const stopped = event('stopped', (body) => body.reason === 'data breakpoint');
        await vscode.commands.executeCommand('workbench.action.debug.continue');
        const { body } = (await stopped).message;
        assert.deepStrictEqual(body.hitBreakpointIds, [id]);
        const at = await focus();
        const call = await session.customRequest('evaluate', { expression: 'call', frameId: at.frameId, context: 'watch' });
        return [body.description, call.result];
    };

    const first = await set({ condition: 'call % 10 == 0' });
    assert.strictEqual(first.verified, true);
    assert.deepStrictEqual(await next(first.id), ['last_call changed from 9 to 10', '10']);
    // Edited, it keeps its id, and its hit count counts every change since
    // it was set.
    const edited = await set({ condition: 'call % 10 == 5', hitCondition: '>=12' });
    assert.deepStrictEqual([edited.id, edited.verified], [first.id, true]);
    assert.deepStrictEqual(await next(first.id), ['last_call changed from 14 to 15', '15']);

    // A condition that cannot be evaluated stops at the next change and says why.
    await set({ condition: 'no_such_value > 1' });
    const explained = event('output', (body) => /could not be evaluated/.test(body.output));
    assert.deepStrictEqual(await next(first.id), ['last_call changed from 15 to 16', '16']);
    assert.strictEqual(
        (await explained).message.body.output,
        `data breakpoint ${first.id} stopped because its condition could not be evaluated: `
            + 'no variable is named `no_such_value` here\n',
    );

    // Conditions that do not parse leave it unverified and unarmed.
    const refused = await set({ condition: 'call = 3' });
    assert.strictEqual(refused.verified, false);
    assert.match(refused.message, /cannot assign; compare with `==`/);
    const bare = await set({ hitCondition: '5' });
    assert.strictEqual(bare.verified, false);
    assert.match(bare.message, /a bare count is ambiguous; write ==5 .* or >=5/);
    await finish();
}

/** A crash stops at the signal, explains it, and ends the program when continued. */
async function crashAtASignal() {
    const exception = answer('exceptionInfo');
    const { body } = await launchToStop({ name: 'vscode-crash', program: fixture('crash-gcc-o0') }, 'exception');
    assert.strictEqual(body.text, 'SIGSEGV');
    // VS Code asks what the exception is, to show it at the stop.
    const info = (await exception).body;
    assert.strictEqual(info.exceptionId, 'SIGSEGV');
    const explained = event('output', (output) => /terminated by SIGSEGV/.test(output.output));
    await finish(139);
    await explained;
}

/**
 * A Go program as VS Code shows it: its goroutines are its threads, named
 * where the program has them; a step stays in its goroutine; a panic the
 * program does not recover stops with the runtime's message; and a stack
 * that crosses from the runtime's own stack to a goroutine's has a label
 * heading each run of frames.
 */
async function goroutinesStepsPanicsAndStacks() {
    const breakpoint = new vscode.FunctionBreakpoint('main.explode');
    vscode.debug.addBreakpoints([breakpoint]);
    const { session, threadId } = await launchToStop({ name: 'vscode-go', program: fixture('panic-go-o0') }, 'function breakpoint');
    // Main's goroutine stopped, and its id is the thread's.
    assert.strictEqual(threadId, 1);
    const { threads } = await session.customRequest('threads');
    const workers = threads.filter((thread) => / main\.worker — chan receive/.test(thread.name));
    assert.strictEqual(workers.length, 5, JSON.stringify(threads));
    assert.ok(workers.some((thread) => /\{job: resize, tenant: "a b"\}/.test(thread.name)), JSON.stringify(workers));

    const stepped = event('stopped', (body) => body.reason === 'step');
    await vscode.commands.executeCommand('workbench.action.debug.stepOver');
    assert.strictEqual((await stepped).message.body.threadId, threadId);
    await focus();
    assert.strictEqual((await topFrame(session, threadId)).name, 'main.explode');

    // Stepping over the panic stops where the runtime reports it. VS Code
    // shows it at the frame that panicked, below the runtime's, where its
    // editor asks what the exception is once someone has used an editor.
    const panicked = event('stopped', (body) => body.reason === 'exception');
    await vscode.commands.executeCommand('workbench.action.debug.stepOver');
    assert.strictEqual((await panicked).message.body.threadId, threadId);
    const shown = await focus();
    const { stackFrames } = await session.customRequest('stackTrace', { threadId });
    const raised = stackFrames.find((frame) => frame.id === shown.frameId);
    assert.strictEqual(raised?.name, 'main.explode', JSON.stringify(stackFrames));
    const exception = await session.customRequest('exceptionInfo', { threadId });
    assert.match(exception.description, /the workers are waiting/);
    await finish(2);

    const switched = new vscode.FunctionBreakpoint('runtime.readmemstats_m');
    vscode.debug.addBreakpoints([switched]);
    const stacks = await launchToStop({ name: 'vscode-go-stacks', program: fixture('stacks-go-o0') }, 'function breakpoint');
    const trace = await stacks.session.customRequest('stackTrace', { threadId: stacks.threadId });
    const labels = trace.stackFrames
        .filter((frame) => frame.presentationHint === 'label')
        .map((frame) => frame.name);
    assert.deepStrictEqual(labels.slice(0, 2), ["on the runtime's stack", "on the task's stack"], JSON.stringify(trace));
    // The program goes on to signal itself, and past that it only ends, so
    // stopping it there ends it however its end races the request's.
    await remove(switched);
    const signalled = event('stopped', (body) => body.reason === 'exception' && body.text === 'SIGUSR1');
    await vscode.commands.executeCommand('workbench.action.debug.continue');
    assert.strictEqual((await signalled).message.body.threadId, stacks.threadId);
    await focus();
    await stop(stacks.session);
}

/**
 * A Python program, launched as its interpreter with the script: a
 * breakpoint in the `.py` file, a step by lines, and an exception the
 * program raises, each shown at the program's frame. 3.14 takes the agent
 * by PEP 768, and 3.12 through the launch bootstrap.
 */
async function pythonBreakpointsStepsAndExceptions() {
    const script = path.join(root, 'tests/fixtures/python/uat.py');
    // The frame VS Code focused at a stop, as the adapter described it.
    const shown = async (session, threadId, item) => {
        const { stackFrames } = await session.customRequest('stackTrace', { threadId });
        return stackFrames.find((frame) => frame.id === item.frameId);
    };
    for (const build of ['3.14', '3.12']) {
        vscode.debug.addBreakpoints([sourceBreakpoint(script, '# AREA_LINE')]);
        const { session, threadId, frame: focusedAtLaunch } = await launchToStop({
            name: `vscode-python-${build}`,
            program: path.join(process.env.MICROSCOPE_CPYTHON, build, 'python'),
            args: ['-P', '-s', '-B', script],
        }, 'breakpoint');
        let frame = await shown(session, threadId, focusedAtLaunch);
        assert.strictEqual(frame?.name, 'area', build);
        assert.strictEqual(frame.line, lineOf(script, '# AREA_LINE') + 1, build);

        const stepped = event('stopped', (body) => body.reason === 'step');
        await vscode.commands.executeCommand('workbench.action.debug.stepOver');
        await stepped;
        frame = await shown(session, threadId, await focus());
        assert.strictEqual(frame?.name, 'area', build);
        assert.strictEqual(frame.line, lineOf(script, '# AREA_LINE') + 2, build);

        // The filter is a checkbox in the breakpoints view, which has no
        // command.
        await session.customRequest('setExceptionBreakpoints', { filters: ['python-raised'] });
        const raised = event('stopped', (body) => body.reason === 'exception');
        await vscode.commands.executeCommand('workbench.action.debug.continue');
        await raised;
        frame = await shown(session, threadId, await focus());
        assert.strictEqual(frame?.name, 'fail', build);
        const exception = await session.customRequest('exceptionInfo', { threadId });
        assert.match(exception.description, /the shape is wrong/, build);
        await session.customRequest('setExceptionBreakpoints', { filters: [] });
        await finish(0);
    }
}

/** The Node builds the Node scenarios run, as the flake names them. */
const NODE_BUILDS = ['26', '24', '22', 'debian-24'];
const nodeOf = (build) => path.join(process.env.MICROSCOPE_NODE, build, 'node');

/**
 * Runs `body` with the adapter for the Node build `build`. Debian's node
 * runs only in the mount namespace its `enter` makes, so its adapter runs
 * there too, started by a wrapper that `prefix`, if given, replaces.
 */
async function withNodeAdapter(build, body, prefix) {
    if (!build.startsWith('debian-')) {
        return body();
    }
    const wrapper = path.join(workspace().uri.fsPath, 'microscope-in-namespace');
    const enter = path.join(path.dirname(nodeOf(build)), 'enter');
    fs.writeFileSync(wrapper, `#!/bin/sh\nexec ${prefix ?? `'${enter}'`} microscope "$@"\n`, { mode: 0o755 });
    const settings = vscode.workspace.getConfiguration('microscope');
    try {
        await settings.update('path', wrapper, vscode.ConfigurationTarget.Global);
        return await body();
    } finally {
        await settings.update('path', undefined, vscode.ConfigurationTarget.Global);
        fs.rmSync(wrapper, { force: true });
    }
}

/** The frame VS Code focused at a stop, as the adapter described it. */
async function shown(session, threadId, item) {
    const { stackFrames } = await session.customRequest('stackTrace', { threadId });
    return stackFrames.find((frame) => frame.id === item.frameId);
}

/** The value of the variable `name` in the scope `scope` of `frame`. */
async function valueIn(session, frame, scope, name) {
    const { scopes } = await session.customRequest('scopes', { frameId: frame.id });
    const found = scopes.find((each) => each.name === scope);
    assert.ok(found, `no ${scope} in ${JSON.stringify(scopes)}`);
    const { variables } = await session.customRequest('variables', { variablesReference: found.variablesReference });
    return variables.find((variable) => variable.name === name)?.value;
}

/**
 * A Node.js program, launched as node with the script: a breakpoint in the
 * `.js` file, a step by lines, the frame's locals by name, and an exception
 * the program throws, each shown at the program's frame, on each Node line.
 */
async function nodeBreakpointsStepsAndExceptions() {
    const script = path.join(root, 'tests/fixtures/node/uat.js');
    for (const build of NODE_BUILDS) await withNodeAdapter(build, async () => {
        vscode.debug.addBreakpoints([sourceBreakpoint(script, '// AREA_LINE')]);
        const { session, threadId, frame: focusedAtLaunch } = await launchToStop({
            name: `vscode-node-${build}`,
            program: nodeOf(build),
            args: [script],
        }, 'breakpoint');
        let frame = await shown(session, threadId, focusedAtLaunch);
        assert.strictEqual(frame?.name, 'area', build);
        assert.strictEqual(frame.line, lineOf(script, '// AREA_LINE') + 1, build);
        assert.ok(frame.column > 1, `${build}: ${JSON.stringify(frame)}`);

        const stepped = event('stopped', (body) => body.reason === 'step');
        await vscode.commands.executeCommand('workbench.action.debug.stepOver');
        await stepped;
        frame = await shown(session, threadId, await focus());
        assert.strictEqual(frame?.name, 'area', build);
        assert.strictEqual(frame.line, lineOf(script, '// RETURN_LINE') + 1, build);

        // The variables view shows the frame's scopes, its locals named.
        assert.strictEqual(await valueIn(session, frame, 'Locals', 'result'), '16', build);
        assert.strictEqual(await valueIn(session, frame, 'Arguments', 'side'), '4', build);

        // The filter is a checkbox in the breakpoints view, which has no
        // command.
        await session.customRequest('setExceptionBreakpoints', { filters: ['javascript-thrown'] });
        const thrown = event('stopped', (body) => body.reason === 'exception');
        await vscode.commands.executeCommand('workbench.action.debug.continue');
        await thrown;
        frame = await shown(session, threadId, await focus());
        assert.strictEqual(frame?.name, 'fail', build);
        assert.strictEqual(frame.line, lineOf(script, '// THROW_LINE') + 1, build);
        const exception = await session.customRequest('exceptionInfo', { threadId });
        assert.match(exception.description, /the shape is wrong/, build);
        await session.customRequest('setExceptionBreakpoints', { filters: [] });
        await finish(0);
    });
}

/**
 * TypeScript, run through tsx's loader and as an esbuild bundle with its
 * map in a file: a breakpoint on a line of the `.ts` file stops there, a
 * step goes to its next line, and an exception stops at the `.ts` line
 * that throws it, on each Node line.
 */
async function nodeTypeScriptThroughSourceMaps() {
    const script = path.join(root, 'tests/fixtures/node/maps/app.ts');
    // Outside /tmp, which Debian's namespace replaces with its own.
    const scratch = path.join(root, 'target/uat-node-maps');
    fs.mkdirSync(scratch, { recursive: true });
    const bundle = path.join(scratch, 'app.js');
    childProcess.execFileSync(process.env.MICROSCOPE_ESBUILD, [
        script, '--bundle', '--platform=node', '--sourcemap', '--log-level=warning', '--format=cjs',
        `--outfile=${bundle}`,
    ]);
    const compilers = [
        ['tsx', ['--import', path.join(process.env.MICROSCOPE_TSX, 'dist/loader.mjs'), script]],
        ['esbuild', [bundle]],
    ];
    try {
        for (const build of NODE_BUILDS) await withNodeAdapter(build, async () => {
            for (const [compiler, args] of compilers) {
                const context = `${build} ${compiler}`;
                vscode.debug.addBreakpoints([sourceBreakpoint(script, '// NORM')]);
                const { session, threadId, frame: focusedAtLaunch } = await launchToStop({
                    name: `vscode-node-${compiler}-${build}`, program: nodeOf(build), args,
                }, 'breakpoint');
                let frame = await shown(session, threadId, focusedAtLaunch);
                assert.strictEqual(frame?.name, 'norm', context);
                assert.strictEqual(frame.source?.path, script, context);
                assert.strictEqual(frame.line, lineOf(script, '// NORM') + 1, context);

                const stepped = event('stopped', (body) => body.reason === 'step');
                await vscode.commands.executeCommand('workbench.action.debug.stepOver');
                await stepped;
                frame = await shown(session, threadId, await focus());
                assert.strictEqual(frame?.source?.path, script, context);
                assert.strictEqual(frame.line, lineOf(script, '// RETURN') + 1, context);
                assert.strictEqual(await valueIn(session, frame, 'Locals', 'squared'), '25', context);

                await session.customRequest('setExceptionBreakpoints', { filters: ['javascript-thrown'] });
                const thrown = event('stopped', (body) => body.reason === 'exception');
                await vscode.commands.executeCommand('workbench.action.debug.continue');
                await thrown;
                frame = await shown(session, threadId, await focus());
                assert.strictEqual(frame?.name, 'fail', context);
                assert.strictEqual(frame.source?.path, script, context);
                assert.strictEqual(frame.line, lineOf(script, '// THROW') + 1, context);
                const exception = await session.customRequest('exceptionInfo', { threadId });
                assert.match(exception.description, /mapped/, context);
                await session.customRequest('setExceptionBreakpoints', { filters: [] });
                await finish(0);
            }
        });
    } finally {
        fs.rmSync(scratch, { recursive: true, force: true });
    }
}

/**
 * A breakpoint in a worker's code stops the worker's thread, not the main
 * one, and a step there stays on it, on each Node line.
 */
async function nodeWorkerStopsOnItsThread() {
    const script = path.join(root, 'tests/fixtures/node/workers.js');
    for (const build of NODE_BUILDS) await withNodeAdapter(build, async () => {
        vscode.debug.addBreakpoints([sourceBreakpoint(script, '// CRUNCH')]);
        const started = event('process');
        const { session, threadId, frame: focusedAtLaunch } = await launchToStop({
            name: `vscode-node-worker-${build}`, program: nodeOf(build), args: [script],
        }, 'breakpoint');
        // The main thread's id is the process's.
        const main = (await started).message.body.systemProcessId;
        assert.notStrictEqual(threadId, main, build);
        const { threads } = await session.customRequest('threads');
        const worker = threads.find((thread) => thread.id === threadId);
        assert.ok(worker, `${build}: ${threadId} in ${JSON.stringify(threads)}`);
        let frame = await shown(session, threadId, focusedAtLaunch);
        assert.strictEqual(frame?.name, 'crunch', build);
        assert.strictEqual(frame.line, lineOf(script, '// CRUNCH') + 1, build);
        assert.strictEqual(await valueIn(session, frame, 'Arguments', 'n'), '6', build);

        const stepped = event('stopped', (body) => body.reason === 'step');
        await vscode.commands.executeCommand('workbench.action.debug.stepOver');
        assert.strictEqual((await stepped).message.body.threadId, threadId, build);
        frame = await shown(session, threadId, await focus());
        assert.strictEqual(frame?.line, lineOf(script, '// AFTER') + 1, build);
        assert.strictEqual(await valueIn(session, frame, 'Locals', 'squared'), '36', build);
        await finish(0);
    });
}

/**
 * A node started without its inspector, attached by its process id: a
 * breakpoint added then stops the main thread, and again a worker the
 * program starts after the attach, on each Node line. Debian's adapter
 * joins the namespace its node was started in.
 */
async function nodeAttachedByProcessId() {
    const script = path.join(root, 'tests/fixtures/node/attach.js');
    for (const build of NODE_BUILDS) {
        const debian = build.startsWith('debian-');
        const command = debian
            ? [path.join(path.dirname(nodeOf(build)), 'enter'), nodeOf(build), script]
            : [nodeOf(build), script];
        // VS Code's own NODE_OPTIONS and the like do not reach the node.
        const target = childProcess.spawn(command[0], command.slice(1), { stdio: ['pipe', 'pipe', 'inherit'], env: {} });
        const exit = new Promise((resolve) => target.once('exit', resolve));
        try {
            await within(`node ${build} to be ready`, new Promise((resolve, reject) => {
                target.stdout.once('data', (data) =>
                    (String(data).startsWith('READY') ? resolve() : reject(new Error(`readiness ${data}`))));
                target.once('error', reject);
            }));
            const prefix = debian ? `'${process.env.MICROSCOPE_UAT_NSENTER}' --target ${target.pid} --user --mount --preserve-credentials` : undefined;
            await withNodeAdapter(build, async () => {
                const breakpoint = sourceBreakpoint(script, '// SERVED');
                const placed = answer('setBreakpoints', (arguments_) => arguments_.source.path === script);
                const confirmed = event('breakpoint', (body) => body.breakpoint.verified);
                confirmed.catch(() => {});
                const attached = event('process');
                assert.ok(await vscode.debug.startDebugging(undefined, {
                    type: 'microscope', request: 'attach', name: `vscode-node-attach-${build}`, pid: String(target.pid),
                }));
                const { session, message: announced } = await attached;
                assert.strictEqual(announced.body.systemProcessId, target.pid, build);
                vscode.debug.addBreakpoints([breakpoint]);
                // V8 confirms the breakpoint while the node runs.
                if (!(await placed).body.breakpoints[0].verified) {
                    await confirmed;
                }
                const line = lineOf(script, '// SERVED') + 1;
                let stopped = event('stopped', (body) => body.reason === 'breakpoint');
                target.stdin.write('x');
                let { message: sent } = await stopped;
                assert.strictEqual(sent.body.threadId, target.pid, build);
                let frame = await shown(session, sent.body.threadId, await focus());
                assert.deepStrictEqual([frame?.name, frame?.line], ['served', line], build);
                assert.strictEqual(await valueIn(session, frame, 'Arguments', 'n'), '41', build);

                stopped = event('stopped', (body) => body.reason === 'breakpoint');
                await vscode.commands.executeCommand('workbench.action.debug.continue');
                ({ message: sent } = await stopped);
                assert.notStrictEqual(sent.body.threadId, target.pid, build);
                frame = await shown(session, sent.body.threadId, await focus());
                assert.deepStrictEqual([frame?.name, frame?.line], ['served', line], build);
                assert.strictEqual(await valueIn(session, frame, 'Arguments', 'n'), '7', build);
                await finish(0);
            }, prefix);
            assert.strictEqual(await within(`node ${build} to exit`, exit), 0, build);
        } finally {
            target.kill();
        }
    }
}

/** Pausing stops every thread, which VS Code lists by name. */
async function pauseAndThreads() {
    const breakpoint = new vscode.FunctionBreakpoint('worker_breakpoint');
    vscode.debug.addBreakpoints([breakpoint]);
    const { session, threadId } = await launchToStop({ name: 'vscode-threads', program: fixture('threads') }, 'function breakpoint');
    // Every thread stopped with the one that hit the breakpoint.
    const { threads } = await session.customRequest('threads');
    assert.strictEqual(threads.length, 3, JSON.stringify(threads));
    assert.ok(threads.some((thread) => thread.id === threadId));
    await finish();

    // VS Code lists a running program's threads when told one started.
    const spinning = event('thread', (body) => body.reason === 'started')
        .then(() => answer('threads'));
    assert.ok(await vscode.debug.startDebugging(undefined, {
        type: 'microscope', request: 'launch', name: 'vscode-spin', program: fixture('spin'), cwd: root,
    }));
    await spinning;
    const spin = vscode.debug.activeDebugSession;
    // Pause, as the call stack's menu on the session does.
    const paused = event('stopped', (body) => body.reason === 'pause');
    await vscode.commands.executeCommand('workbench.action.debug.pause', { sessionId: spin.id });
    await paused;
    await focus();
    await stop(spin);
}

/** Breakpoints in a library wait for it, and its sources come and go with it. */
async function librariesAndTheirSources() {
    const file = source('shared/library.c');
    const breakpoint = sourceBreakpoint(file, 'return *dso_pointer');
    const after = new vscode.FunctionBreakpoint('after_unload');
    vscode.debug.addBreakpoints([breakpoint, after]);
    const pending = answer('setBreakpoints', (arguments_) => arguments_.source.path === file);
    const { session } = await launchToStop({
        name: 'vscode-library', program: fixture('globals-shared'), stopOnEntry: true,
    }, 'entry');
    assert.strictEqual((await pending).body.breakpoints[0].reason, 'pending');
    // The loaded scripts view lists the sources, and follows them.
    const listed = answer('loadedSources');
    await vscode.commands.executeCommand('workbench.debug.loadedScriptsView.focus');
    assert.ok(!(await listed).body.sources.some((entry) => entry.path === file));
    const loaded = event('loadedSource', (body) => body.reason === 'new' && body.source.path === file);
    const resolved = event('breakpoint', (body) => body.breakpoint.verified && body.breakpoint.source?.path === file);
    const hit = event('stopped', (body) => body.reason === 'breakpoint');
    await vscode.commands.executeCommand('workbench.action.debug.continue');
    await loaded;
    await resolved;
    const { message: stopped } = await hit;
    await focus();
    assert.strictEqual((await topFrame(session, stopped.body.threadId)).source.path, file);
    const unloaded = event('loadedSource', (body) => body.reason === 'removed' && body.source.path === file);
    const unloadStop = event('stopped', (body) => body.reason === 'function breakpoint');
    await vscode.commands.executeCommand('workbench.action.debug.continue');
    await unloaded;
    await unloadStop;
    await focus();
    await stop(session);
    vscode.debug.removeBreakpoints(vscode.debug.breakpoints);
}

async function runInTheIntegratedTerminal() {
    const breakpoint = sourceBreakpoint(source('process-environment.c'), 'if (fgets(line');
    vscode.debug.addBreakpoints([breakpoint]);
    const stopped = event('stopped', (body) => body.reason === 'breakpoint');
    const process = event('process');
    assert.ok(await vscode.debug.startDebugging(undefined, {
        type: 'microscope', request: 'launch', name: 'vscode-terminal', program: fixture('process-environment'),
        args: ['one'], console: 'integratedTerminal', cwd: root,
    }));
    await stopped;
    await focus();
    assert.strictEqual((await process).message.body.startMethod, 'launch');
    const terminal = vscode.window.terminals.find((candidate) => candidate.name.includes('process-environment'));
    assert.ok(terminal, `a terminal among ${vscode.window.terminals.map((candidate) => candidate.name)}`);
    await remove(breakpoint);
    const exited = event('exited');
    const ended = terminated();
    await vscode.commands.executeCommand('workbench.action.debug.continue');
    // The program reads its line from the terminal.
    terminal.sendText('typed');
    assert.strictEqual((await exited).message.body.exitCode, 2);
    await ended;
    terminal.dispose();
}

/** Spawns the attach fixture and waits until it is ready to be attached to. */
async function attachTarget() {
    const target = childProcess.spawn(fixture('attach'), [], { stdio: ['pipe', 'pipe', 'inherit'] });
    const exit = new Promise((resolve) => target.once('exit', resolve));
    await new Promise((resolve, reject) => {
        target.stdout.once('data', (data) =>
            (String(data).startsWith('READY') ? resolve() : reject(new Error(`readiness ${data}`))));
        target.once('error', reject);
    });
    return { target, exit };
}

async function attachToAProcess() {
    const { target, exit } = await attachTarget();
    try {
        const process = event('process');
        assert.ok(await vscode.debug.startDebugging(undefined, {
            type: 'microscope', request: 'attach', name: 'vscode-attach', pid: String(target.pid),
        }));
        const { session } = await process;
        // A breakpoint added while the process runs is placed at once.
        const breakpoint = new vscode.FunctionBreakpoint('attach_breakpoint');
        const placed = answer('setFunctionBreakpoints', (arguments_) => arguments_.breakpoints.length === 1);
        vscode.debug.addBreakpoints([breakpoint]);
        assert.strictEqual((await placed).body.breakpoints[0].verified, true);
        // Restarting detaches, which leaves the process running, and attaches
        // to it again, keeping the breakpoints.
        const detached = answer('disconnect', (arguments_) => arguments_.restart === true);
        const again = event('process', (body) => body.systemProcessId === target.pid);
        await vscode.commands.executeCommand('workbench.action.debug.restart');
        assert.ok((await detached).success);
        await again;
        const stopped = event('stopped', (body) => body.reason === 'function breakpoint');
        target.stdin.write('x');
        const { message: sent } = await stopped;
        assert.strictEqual((await topFrame(session, sent.body.threadId)).name, 'attach_breakpoint');
        await remove(breakpoint);
        // Disconnecting leaves the process to finish on its own.
        const ended = terminated();
        await vscode.debug.stopDebugging(session);
        await ended;
        assert.strictEqual(await within('the process to exit', exit), 23);
    } finally {
        target.kill();
    }
}

/**
 * ${command:pickProcess} lists the user's processes, or with a program, only
 * those running it, which leaves the target as the one item to accept.
 */
async function pickAProcess() {
    const { target, exit } = await attachTarget();
    try {
        const { processes } = extension();
        const listed = processes();
        assert.deepStrictEqual(listed.filter((entry) => entry.pid === target.pid).map(
            ({ name, commandLine, executable }) => ({ name, commandLine, executable }),
        ), [{ name: 'attach', commandLine: fixture('attach'), executable: fixture('attach') }]);
        assert.ok(!listed.some((entry) => entry.pid === process.pid), 'the extension host is not listed');

        const attached = event('process');
        const started = vscode.debug.startDebugging(undefined, {
            type: 'microscope', request: 'attach', name: 'vscode-pick', program: fixture('attach'),
            pid: '${command:pickProcess}',
        });
        let settled = false;
        started.then(() => { settled = true; }, () => { settled = true; });
        // Accept the only process the picker lists once it opens.
        await until('the picker to be accepted', () => {
            if (!settled) {
                vscode.commands.executeCommand('workbench.action.acceptSelectedQuickOpenItem');
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

/**
 * With follow, VS Code starts a session for a forked process at the
 * adapter's request, which the extension connects back to the same
 * adapter. The child stops at the breakpoint after its fork, having run
 * nothing untraced, and each session ends as its process does. Process
 * stops keep the two stops independent of which comes first.
 */
async function followAForkedChild() {
    const breakpoint = new vscode.FunctionBreakpoint('shared_work');
    vscode.debug.addBreakpoints([breakpoint]);
    const stopIn = (child) => message(`the ${child ? 'child' : 'parent'} to stop`, (session, sent) =>
        sent.type === 'event' && sent.event === 'stopped' && sent.body.reason === 'function breakpoint'
            && (session.parentSession !== undefined) === child);
    const parentStopped = stopIn(false);
    const childStopped = stopIn(true);
    const asked = message('the adapter to ask for a child session', (_, sent) =>
        sent.type === 'request' && sent.command === 'startDebugging');
    // VS Code answers once the child session has attached.
    const started = message('VS Code to answer startDebugging', (_, sent) =>
        sent.type === 'response' && sent.command === 'startDebugging' && sent.success, 'client');
    assert.ok(await vscode.debug.startDebugging(undefined, {
        type: 'microscope', request: 'launch', name: 'vscode-fork', program: fixture('fork'), cwd: root,
        follow: { mode: 'all', stop: 'process' },
    }));
    const asking = (await asked).message.arguments;
    assert.strictEqual(asking.request, 'attach');
    const { session: parent, message: parentStop } = await parentStopped;
    const { session: child, message: childStop } = await childStopped;
    await started;
    assert.strictEqual(child.parentSession.id, parent.id);
    assert.strictEqual(child.configuration.pid, asking.configuration.pid);
    assert.strictEqual(child.configuration.__microscopeMember.number, 2);
    assert.strictEqual(child.name, asking.configuration.name);
    for (const [session, stop] of [[parent, parentStop], [child, childStop]]) {
        assert.strictEqual((await topFrame(session, stop.body.threadId)).name, 'shared_work', session.name);
    }
    await remove(breakpoint);

    // The child runs to its end, and then its parent, which waited for it.
    for (const [session, stop] of [[child, childStop], [parent, parentStop]]) {
        const exited = message(`${session.name} to exit`, (from, sent) =>
            from.id === session.id && sent.type === 'event' && sent.event === 'exited');
        const ended = terminated(session.name);
        await vscode.commands.executeCommand('workbench.action.debug.continue', undefined, {
            sessionId: session.id, threadId: stop.body.threadId,
        });
        assert.strictEqual((await exited).message.body.exitCode, 0, session.name);
        await ended;
    }
}

async function openACoreDump() {
    const stopped = event('stopped', (body) => body.reason === 'exception');
    const capabilities = event('capabilities');
    assert.ok(await vscode.debug.startDebugging(undefined, {
        type: 'microscope', request: 'attach', name: 'vscode-core', coreFile: fixture('crash-gcc-o0-segv.core'),
        program: fixture('crash-gcc-o0'),
    }));
    const { session, message: sent } = await stopped;
    assert.match(sent.body.text, /SIGSEGV/);
    assert.strictEqual((await capabilities).message.body.capabilities.supportsRestartRequest, false);
    const trace = await session.customRequest('stackTrace', { threadId: sent.body.threadId });
    assert.ok(trace.stackFrames.some((frame) => frame.name === 'main'), JSON.stringify(trace));
    await focus();
    // A core dump has nothing to run again, so VS Code restarts by opening
    // it again with a new adapter, in the same session.
    const disconnected = request('disconnect', (arguments_) => arguments_.restart === true);
    const reopened = event('stopped', (body) => body.reason === 'exception');
    await vscode.commands.executeCommand('workbench.action.debug.restart');
    await disconnected;
    const { session: again } = await reopened;
    // The new adapter numbers its frames from the start again.
    focused = undefined;
    await focus();
    const ended = terminated();
    await vscode.debug.stopDebugging(again);
    await ended;
}

/**
 * Configurations that cannot start say why without starting anything, and
 * a folder's programs are offered to launch.
 */
async function refuseBadConfigurationsAndOfferPrograms() {
    for (const [name, configuration, reason] of [
        ['vscode-no-program', { request: 'launch', program: fixture('no-such-program') }, /failed to load .*no-such-program/],
        ['vscode-bad-arguments', { request: 'launch', program: fixture('basic'), args: 'one' }, /invalid launch configuration at args/],
        ['vscode-no-process', { request: 'attach', pid: 999999999 }, /process 999999999/],
        ['vscode-no-core', { request: 'attach', coreFile: fixture('no-such.core') }, /no-such\.core/],
    ]) {
        const refused = response(configuration.request, (reply) => !reply.success);
        const disconnected = request('disconnect');
        const started = vscode.debug.startDebugging(undefined, { type: 'microscope', name, ...configuration });
        assert.match((await refused).message.message, reason, name);
        // VS Code shows the refusal in a dialog, which its test mode refuses
        // to open, and ends the session it never started.
        await assert.rejects(within('the refusal to be shown', started), (error) =>
            /refused to show dialog/.test(error.message) && reason.test(error.message));
        await disconnected;
        assert.strictEqual(vscode.debug.activeDebugSession, undefined, name);
    }

    const folder = workspace().uri.fsPath;
    const build = path.join(folder, 'build');
    fs.mkdirSync(path.join(build, 'deps'), { recursive: true });
    try {
        fs.copyFileSync(fixture('basic'), path.join(build, 'basic'));
        fs.chmodSync(path.join(build, 'basic'), 0o755);
        // Neither a library, a dependency's build, nor a script is a program.
        fs.copyFileSync(fixture('libglobals.so'), path.join(build, 'libglobals.so'));
        fs.chmodSync(path.join(build, 'libglobals.so'), 0o755);
        fs.copyFileSync(fixture('basic'), path.join(build, 'deps', 'basic-test'));
        fs.chmodSync(path.join(build, 'deps', 'basic-test'), 0o755);
        fs.writeFileSync(path.join(build, 'script'), '#!/bin/sh\n', { mode: 0o755 });
        const offered = extension().provideDebugConfigurations(workspace());
        assert.deepStrictEqual(offered.map((configuration) => configuration.program), ['${workspaceFolder}/build/basic']);
        const exited = event('exited');
        const ended = terminated(offered[0].name);
        assert.ok(await vscode.debug.startDebugging(workspace(), offered[0]));
        assert.strictEqual((await exited).message.body.exitCode, 0);
        await ended;
    } finally {
        fs.rmSync(build, { recursive: true, force: true });
    }
}

exports.run = async function run() {
    // MICROSCOPE_UAT_ONLY names the scenarios to run, separated by commas.
    const only = process.env.MICROSCOPE_UAT_ONLY?.split(',');
    try {
        for (const scenario of [
            createALaunchJson, findTheAdapterFromTheSetting, logTheProtocolToAFile, launchStepInspectAndRestart,
            stepIntoByInstructionAndRunToCursor, conditionsHitCountsLogpointsAndRefusals,
            consoleWatchHoverHexadecimalAndInlineValues, completionsAndLocations, dataBreakpointsAndTheirModes,
            dataBreakpointConditions, crashAtASignal, goroutinesStepsPanicsAndStacks,
            pythonBreakpointsStepsAndExceptions, nodeBreakpointsStepsAndExceptions,
            nodeTypeScriptThroughSourceMaps, nodeWorkerStopsOnItsThread, nodeAttachedByProcessId,
            pauseAndThreads, librariesAndTheirSources, runInTheIntegratedTerminal, attachToAProcess,
            pickAProcess, followAForkedChild, openACoreDump, refuseBadConfigurationsAndOfferPrograms,
        ].filter((scenario) => only === undefined || only.includes(scenario.name))) {
            console.log(`uat: ${scenario.name}`);
            await scenario();
            console.log(`uat: ${scenario.name} passed`);
        }
    } finally {
        save();
    }
};
