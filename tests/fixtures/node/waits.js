// Main and WORKERS threads wait 2 ms at a time until stdin releases them.
// Startup snapshots must find the code V8 just mapped for each isolate.
//
// node waits.js WORKERS

'use strict';

const fs = require('node:fs');
const threads = require('node:worker_threads');
const truth = require('./truth.js');

const cell = new Int32Array(threads.isMainThread ? new SharedArrayBuffer(8) : threads.workerData);

if (threads.isMainThread) {
  for (let i = 0; i < Number(process.argv[2]); i++) {
    new threads.Worker(__filename, { workerData: cell.buffer });
  }
  process.stdin.once('data', () => {
    Atomics.store(cell, 1, 1);
    process.stdin.destroy();
  });
}

truth.report('ready', Number(fs.readlinkSync('/proc/thread-self').split('/').pop()));

function wait() {
  if (Atomics.load(cell, 1) !== 0) return;
  Atomics.wait(cell, 0, 0, 2);
  setImmediate(wait);
}
wait();
