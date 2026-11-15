






'use strict';

const fs = require('node:fs');
const path = require('node:path');
const threads = require('node:worker_threads');
const microscope = require(path.join(__dirname, '..', '..', '..', 'sdk', 'node'));

if (microscope.problem !== null) {
  throw new Error(microscope.problem);
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



microscope.frame('started');
fs.readSync(0, Buffer.alloc(1));

for (let round = 0; round < 3; round++) {
  microscope.label('round', round, () => {
    const rows = load(round + 1);
    microscope.plot('rows', rows.length);
  });
  microscope.message('said', () => `round ${round}`);
}

const worker = new threads.Worker(__filename);
worker.once('message', () => {



  for (let number = 0; number < 4100; number++) {
    microscope.wrap(`request ${number}`, () => {});
  }
  process.stdout.write('PROBES DONE\n');
});
