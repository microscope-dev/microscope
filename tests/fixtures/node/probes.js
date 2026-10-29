// A program instrumented with microscope's Node package: it waits until
// microscope listens, then marks its work every way the package can, on
// the main thread and a worker, and ends by defining more sites than the
// library holds. MICROSCOPE_NODE_ADDON names the package's addon and
// MICROSCOPE_PROBES_LIBRARY the probes library.

'use strict';

const path = require('node:path');
const threads = require('node:worker_threads');
const microscope = require(path.join(__dirname, '..', '..', '..', 'sdk', 'node'));

if (microscope.problem !== null) {
  throw new Error(microscope.problem);
}

function sleep(milliseconds) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, milliseconds);
}

if (!threads.isMainThread) {
  microscope.zone('worker', () => microscope.plot('worker rows', 7));
  threads.parentPort.postMessage('done');
  return;
}

const load = microscope.wrap('load', (rows) =>
  microscope.zone('parse', () => Array.from({length: rows}, (_, index) => index)));

for (const suspending of [async () => {}, function* numbers() {}]) {
  let refused = false;
  try {
    microscope.zone('suspends', suspending);
  } catch (error) {
    refused = error instanceof TypeError;
  }
  if (!refused) {
    throw new Error('a zone spanned what a function awaits or yields');
  }
}

// A recording that starts the program switches it on at once, but its marks
// are safe only once microscope has found it.
microscope.frame('started');
const deadline = Date.now() + 30000;
while (!microscope.found()) {
  if (Date.now() > deadline) {
    throw new Error('microscope never found the program');
  }
  sleep(1);
}

for (let round = 0; round < 3; round++) {
  microscope.label('round', round, () => {
    const rows = load(round + 1);
    microscope.plot('rows', rows.length);
  });
  microscope.message('said', () => `round ${round}`);
}

const worker = new threads.Worker(__filename);
worker.once('message', () => {
  // A name made anew for each use is a site of its own, until every site
  // the library's table holds is taken; a wrapped function never called
  // defines its site and writes nothing.
  for (let number = 0; number < 4100; number++) {
    microscope.wrap(`request ${number}`, () => {});
  }
  process.stdout.write('PROBES DONE\n');
});
