// Threads that each block, for good, in native code that JavaScript called,
// each in a way of its own, for live reads of their frames
// (tests/node/profile.rs). Each prints its frames with truth.blocking() on
// the line that blocks, as the checkpoint its state names:
//
//   debugger  the main thread, paused at a debugger statement by the
//             control worker's inspector session
//   control   a worker that holds that session, in fs.readSync on standard
//             input; a line there ends the session, which resumes the main
//             thread, and it lets every worker go on and exits
//   reviver   in fs.readSync, from a JSON.parse reviver
//   proxy     in fs.readSync, from a Proxy's ownKeys trap Object.keys runs
//   sort      in fs.readSync, from an Array.prototype.sort comparator
//   nested    in fs.readSync, from a callback a Node-API function calls:
//             JavaScript, native, JavaScript, native
//   execfile  in child_process.execFileSync, of a node that exits once
//             its parent has
//   atomics   in Atomics.wait
//   napi      in a Node-API function's open(2) of a FIFO no one writes
//   fast      in a fast API call, from optimized code, which blocks in
//             read(2); it reports, as `fast CALLS LINE`, how many calls took
//             the fast path and the line of the call, which runs in
//             fastCaller, a frame CallSite does not report here
//
// and three that report only their native ids and when they began, TIME,
// process.hrtime.bigint(): `idle ID TIME`, a worker idle in its event loop
// from then on; `spin ID TIME`, a worker that runs JavaScript from then on;
// and `wasm ID TIME`, a worker blocked for good in Atomics.wait from a
// JavaScript function that a WebAssembly function called.
//
// node --allow-natives-syntax live.js QUIET OPENED ADDON FRAMELESS FAST
//
// QUIET is a FIFO each reader opens to read and write, so that it never
// sees an end; OPENED a FIFO no one opens to write; ADDON, FRAMELESS, and
// FAST the built node-addon.node, node-frameless.node, and the build's
// node-fast-ENTRY.node.

'use strict';

const childProcess = require('node:child_process');
const fs = require('node:fs');
const threads = require('node:worker_threads');
const truth = require('./truth.js');

const paths = threads.isMainThread ? process.argv.slice(2) : threads.workerData.paths;
const [quiet, opened, addonPath, framelessPath, fastPath] = paths;
const buffer = Buffer.alloc(1);

// A child that exits once its parent has, which leaves it to init.
const CHILD = 'const parent = process.ppid; ' +
  'setInterval(() => { if (process.ppid !== parent) process.exit(); }, 10);';

function nativeId() {
  return Number(fs.readlinkSync('/proc/thread-self').split('/').pop());
}

function parkedReviver() {
  const fd = fs.openSync(quiet, 'r+');
  truth.blocking('reviver'), fs.readSync(fd, buffer, 0, 1, null);
}

function parkedProxy() {
  const fd = fs.openSync(quiet, 'r+');
  truth.blocking('proxy'), fs.readSync(fd, buffer, 0, 1, null);
}

function parkedSort() {
  const fd = fs.openSync(quiet, 'r+');
  truth.blocking('sort'), fs.readSync(fd, buffer, 0, 1, null);
}

function parkedNested() {
  const fd = fs.openSync(quiet, 'r+');
  truth.blocking('nested'), fs.readSync(fd, buffer, 0, 1, null);
}

function parkedExecFile() {
  try {
    truth.blocking('execfile'), childProcess.execFileSync(process.execPath, ['-e', CHILD]);
  } catch {
    // release() killed the child.
  }
}

function parkedAtomics() {
  const cell = new Int32Array(new SharedArrayBuffer(4));
  truth.blocking('atomics'), Atomics.wait(cell, 0, 0);
}

function parkedNapi() {
  const frameless = require(framelessPath);
  truth.blocking('napi'), frameless.open(opened);
}

function fastCaller(fast, fd) {
  return fast.read(fd); // the fast call
}

function parkedFast() {
  const fast = require(fastPath);
  %PrepareFunctionForOptimization(fastCaller);
  fastCaller(fast, -1);
  fastCaller(fast, -1);
  %OptimizeFunctionOnNextCall(fastCaller);
  fastCaller(fast, -1);
  const marker = '// the fast' + ' call';
  const line = fs.readFileSync(__filename, 'utf8').split('\n')
    .findIndex((text) => text.includes(marker)) + 1;
  truth.report('fast', fast.fastCalls(), line);
  const fd = fs.openSync(quiet, 'r+');
  truth.blocking('fast'), fastCaller(fast, fd);
}

function parkedControl(session) {
  truth.blocking('control'), fs.readSync(0, buffer, 0, 1, null);
  // Its debugger, which no session enables any longer, lets it go.
  session.disconnect();
}

function parkedDebugger() {
  truth.blocking('debugger'); debugger;
}

const states = {
  reviver() {
    JSON.parse('{"a": 1}', function revive(key, value) {
      if (key === 'a') {
        parkedReviver();
      }
      return value;
    });
  },
  proxy() {
    Object.keys(new Proxy({}, {
      ownKeys() {
        parkedProxy();
        return [];
      },
    }));
  },
  sort() {
    [2, 1].sort(function compare(a, b) {
      parkedSort();
      return a - b;
    });
  },
  nested() {
    require(addonPath).inspect({}, 'text', function nestedCallback() {
      parkedNested();
    });
  },
  execfile: parkedExecFile,
  atomics: parkedAtomics,
  napi: parkedNapi,
  fast: parkedFast,
  control() {
    const inspector = require('node:inspector');
    const session = new inspector.Session();
    session.connectToMainThread();
    // The session's reply holds no handle that keeps the worker alive.
    setInterval(() => {}, 1 << 30);
    session.post('Debugger.enable', () => {
      threads.parentPort.postMessage('enabled');
      parkedControl(session);
    });
  },
  idle() {
    truth.report('idle', nativeId(), process.hrtime.bigint());
    setInterval(() => {}, 1 << 30);
  },
  wasm() {
    // (module (import "m" "f" (func)) (func (export "run") call 0))
    const bytes = new Uint8Array([
      0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
      0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
      0x02, 0x07, 0x01, 0x01, 0x6d, 0x01, 0x66, 0x00, 0x00,
      0x03, 0x02, 0x01, 0x00,
      0x07, 0x07, 0x01, 0x03, 0x72, 0x75, 0x6e, 0x00, 0x01,
      0x0a, 0x06, 0x01, 0x04, 0x00, 0x10, 0x00, 0x0b,
    ]);
    const cell = new Int32Array(new SharedArrayBuffer(4));
    const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes), {
      m: {f: function fromWasm() { Atomics.wait(cell, 0, 0); }},
    });
    truth.report('wasm', nativeId(), process.hrtime.bigint());
    instance.exports.run();
  },
  spin() {
    truth.report('spin', nativeId(), process.hrtime.bigint());
    let count = 0;
    for (;;) {
      count = (count + 1) | 0;
    }
  },
};

/**
 * Lets each worker blocked in native code go on, which the process's exit
 * waits for, and exits: each reader of QUIET reads a byte, the open of
 * OPENED returns, and each child is killed.
 */
function release() {
  fs.writeSync(fs.openSync(quiet, 'r+'), Buffer.alloc(64));
  fs.closeSync(fs.openSync(opened, 'w'));
  for (const task of fs.readdirSync('/proc/self/task')) {
    const children = fs.readFileSync(`/proc/self/task/${task}/children`, 'utf8');
    for (const child of children.split(' ').filter(Boolean)) {
      process.kill(Number(child));
    }
  }
  process.exit(0);
}

if (threads.isMainThread) {
  for (const state of Object.keys(states)) {
    const worker = new threads.Worker(__filename, {workerData: {state, paths}});
    if (state === 'control') {
      worker.once('message', () => {
        parkedDebugger();
        release();
      });
    }
  }
} else {
  states[threads.workerData.state]();
}
