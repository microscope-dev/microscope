



'use strict';
const { Worker, isMainThread, parentPort, workerData } = require('worker_threads');

function crunch(n) {
  const squared = n * n; // CRUNCH
  return squared + 1; // AFTER
}

function risky() {
  try {
    throw new RangeError('in the worker');
  } catch {
    return 0;
  }
}

if (isMainThread) {
  const both = process.argv[2] === 'both';

  const flags = new Int32Array(new SharedArrayBuffer(8));
  const worker = new Worker(__filename, { workerData: { n: 6, flags, both } });
  worker.on('message', (value) => console.log('RESULT', value));
  worker.on('exit', (code) => console.log('EXIT', code));
  if (both) {
    while (Atomics.load(flags, 0) === 0) {}
    Atomics.store(flags, 1, 1);
    console.log('MAIN', crunch(2));
  }
} else {
  const { n, flags, both } = workerData;
  risky();
  if (both) {
    Atomics.store(flags, 0, 1);
    while (Atomics.load(flags, 1) === 0) {}
  }
  parentPort.postMessage(crunch(n));
}
