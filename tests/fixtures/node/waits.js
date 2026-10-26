// Threads that block from their start: the main thread and WORKERS
// workers each wait 2 ms at a time in Atomics.wait for SECONDS, so that a
// recording's first live reads find each thread asleep in code V8 mapped
// moments before.
//
// node waits.js WORKERS SECONDS

'use strict';

const threads = require('node:worker_threads');

const [workers, seconds] = threads.isMainThread
  ? process.argv.slice(2).map(Number)
  : threads.workerData;
const cell = new Int32Array(new SharedArrayBuffer(4));

if (threads.isMainThread) {
  for (let i = 0; i < workers; i++) {
    new threads.Worker(__filename, { workerData: [workers, seconds] });
  }
}

const end = Date.now() + seconds * 1000;
while (Date.now() < end) {
  Atomics.wait(cell, 0, 0, 2);
}
