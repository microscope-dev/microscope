// The oracle for microscope's JavaScript run control: runs a fixture under
// Node's own inspector, driven by a plain CDP client, and prints where V8
// paused. Usage: node cdp_oracle.js PLAN FIXTURE [ARGS...], where PLAN is
// JSON: { "breakpoints": [{ "line": 3, "condition": "n == 2" }, ...],
// "commands": ["Debugger.stepInto", ...], "skip": ["/vendor/", ...],
// "enter": false,
// "exceptions": "none" | "caught" | "uncaught" }. Each pause prints
// `PAUSE <reason> <function> <line> <column>` (one-based), and the command
// after it is the next of `commands`, then `Debugger.resume`.
'use strict';
const { spawn } = require('child_process');
const path = require('path');

const [plan, fixture, ...rest] = process.argv.slice(2);
const { breakpoints = [], commands = [], exceptions = 'none', skip = [], enter = false } =
  JSON.parse(plan);
const child = spawn(process.execPath, ['--inspect-brk=127.0.0.1:0', fixture, ...rest], {
  stdio: ['ignore', 'inherit', 'pipe'],
});

let errors = '';
child.stderr.setEncoding('utf8');
child.stderr.on('data', (chunk) => {
  errors += chunk;
  const found = errors.match(/ws:\/\/\S+/);
  if (found && !socket) connect(found[0]);
});

let socket;
let next = 1;
const answers = new Map();
const scripts = new Map();
const send = (method, params = {}) =>
  new Promise((resolve) => {
    const id = next++;
    answers.set(id, resolve);
    socket.send(JSON.stringify({ id, method, params }));
  });

let started = false;
let remaining = [...commands];

function connect(url) {
  socket = new WebSocket(url);
  socket.onmessage = ({ data }) => {
    const message = JSON.parse(data);
    if (message.id) {
      answers.get(message.id)?.(message.result ?? message.error);
      answers.delete(message.id);
    } else if (message.method === 'Debugger.scriptParsed') {
      scripts.set(message.params.scriptId, message.params.url);
    } else if (message.method === 'Debugger.paused') {
      paused(message.params);
    } else if (message.method === 'NodeRuntime.waitingForDisconnect') {
      socket.close();
    }
  };
  socket.onopen = async () => {
    await send('NodeRuntime.notifyWhenWaitingForDisconnect', { enabled: true });
    await send('Debugger.enable');
    await send('Debugger.setPauseOnExceptions', { state: exceptions });
    const pattern = `${path.basename(fixture).replace(/[.]/g, '\\.')}$`;
    for (const { line, condition = '' } of breakpoints) {
      await send('Debugger.setBreakpointByUrl', { urlRegex: pattern, lineNumber: line - 1, condition });
    }
    await send('Runtime.runIfWaitingForDebugger');
  };
}

function paused({ reason, callFrames, data }) {
  // --inspect-brk pauses before the first statement; the plan starts after.
  if (!started) {
    started = true;
    send('Debugger.resume');
    return;
  }
  const [top] = callFrames;
  // A step never ends in Node's own scripts, unless it enters a runtime's
  // code, or in a skipped one: it goes on into the next statement of the
  // program. V8's blackboxing cannot skip Node's, since the builtins in
  // Node's snapshot have no context.
  const url = scripts.get(top.location.scriptId) ?? '';
  const skipped = (url.startsWith('node:') && !enter) || skip.some((part) => url.includes(part));
  if (reason === 'step' && skipped) {
    send('Debugger.stepInto');
    return;
  }
  // An exception caught in code steps pass through is that code's own.
  const exception = reason === 'exception' || reason === 'promiseRejection';
  if (exception && skipped && !data?.uncaught) {
    send('Debugger.resume');
    return;
  }
  const { lineNumber, columnNumber } = top.location;
  const uncaught = exception ? ` ${data?.uncaught ? 'uncaught' : 'caught'}` : '';
  console.log(`PAUSE ${reason} ${top.functionName || '-'} ${lineNumber + 1} ${columnNumber + 1}${uncaught}`);
  send(remaining.shift() ?? 'Debugger.resume');
}
