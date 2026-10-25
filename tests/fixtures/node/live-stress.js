// Threads that block in native code for about a millisecond at a time, at
// sites of known frames, as fast as they can, while V8 collects garbage,
// moves objects, and deoptimizes code under them, for live reads that must
// find each thread at one of its sites or leave it out
// (tests/node/live.rs).
//
// node --allow-natives-syntax [V8 flags] live-stress.js ADDON FAST WORKERS SECONDS
//
// ADDON and FAST are the built node-addon.node and the build's
// node-fast-ENTRY.node. The main thread prints `waiting` and waits for a
// line on its standard input, sent once the recording reads it, so that
// loading the program's debug information takes none of the run. Then the
// main thread and WORKERS workers each loop over
// the sites for SECONDS, in phases of PHASE_MS each: each phase calls the
// sites through a function of its own, phase0 to phase3 in turn, so that
// the frames say which phase a thread was in. Each thread prints, the first
// time it blocks at a site in a phase, the frames with
// truth.blocking('SITE PHASE TID'), on the line that blocks; `phase TID
// PHASE TIME` as it begins a phase; once it has blocked at every site in
// every phase, `ready TID TIME`; and as it stops, `done TID TIME`, with
// TIME process.hrtime.bigint(). Between ready and done, the thread runs only
// these sites and the code that leads to them.
//
// The sites, each of which blocks in native code that JavaScript called:
//
//   wait     Atomics.wait with a timeout
//   nested   Atomics.wait from a callback a Node-API function calls
//   reviver  Atomics.wait from a JSON.parse reviver
//   sort     Atomics.wait from an Array.prototype.sort comparator
//   fast     a nap in a fast API call, from optimized code, or in its slow
//            callback, from code V8 deoptimized
//   mixed    Atomics.wait, from code whose argument types change, which V8
//            deoptimizes as they do

'use strict';

const fs = require('node:fs');
const threads = require('node:worker_threads');
const truth = require('./truth.js');

const [addonPath, fastPath, workers, seconds] = threads.isMainThread
  ? process.argv.slice(2)
  : threads.workerData;
const addon = require(addonPath);
const fast = require(fastPath);

// How long each site blocks, in milliseconds.
const BLOCK_MS = 1;

// How long each phase lasts, in milliseconds, and how many there are.
const PHASE_MS = 400;
const PHASES = 4;

const tid = Number(fs.readlinkSync('/proc/thread-self').split('/').pop());
const cell = new Int32Array(new SharedArrayBuffer(4));
const seen = new Set();
let garbage = [];
let phase = 0;

/**
 * Whether this is the thread's first block at `site` in this phase, which
 * it reports, as the name it reports it by.
 */
function first(site) {
  const name = `${site} ${phase} ${tid}`;
  if (seen.has(name)) {
    return null;
  }
  seen.add(name);
  return name;
}

// Objects of shapes that change, kept a while, so that the collector moves
// them and code that read them deoptimizes.
function churn(i) {
  const object = i % 3 === 0 ? {a: i, b: [i, i + 1]} : i % 3 === 1 ? {s: `x${i}`} : [i, 'y', {z: i}];
  garbage.push(object);
  if (garbage.length > 2000) {
    garbage = [];
  }
  return object;
}

function siteWait(i) {
  const name = first('wait'); if (name) truth.blocking(name), Atomics.wait(cell, 0, 0, BLOCK_MS); else Atomics.wait(cell, 0, 0, BLOCK_MS);
  return churn(i);
}

function nestedInner(i) {
  const name = first('nested'); if (name) truth.blocking(name), Atomics.wait(cell, 0, 0, BLOCK_MS); else Atomics.wait(cell, 0, 0, BLOCK_MS);
  return churn(i);
}

function siteNested(i) {
  addon.inspect({}, 'text', function nestedCallback() {
    nestedInner(i);
  });
  return churn(i);
}

function reviverInner(i) {
  const name = first('reviver'); if (name) truth.blocking(name), Atomics.wait(cell, 0, 0, BLOCK_MS); else Atomics.wait(cell, 0, 0, BLOCK_MS);
  return churn(i);
}

function siteReviver(i) {
  JSON.parse('{"a": 1}', function revive(key, value) {
    if (key === 'a') {
      reviverInner(i);
    }
    return value;
  });
  return churn(i);
}

function sortInner(i) {
  const name = first('sort'); if (name) truth.blocking(name), Atomics.wait(cell, 0, 0, BLOCK_MS); else Atomics.wait(cell, 0, 0, BLOCK_MS);
  return churn(i);
}

function siteSort(i) {
  [2, 1].sort(function compare(a, b) {
    sortInner(i);
    return a - b;
  });
  return churn(i);
}

function siteFast(i) {
  const name = first('fast'); if (name) truth.blocking(name), fast.nap(BLOCK_MS * 1000); else fast.nap(BLOCK_MS * 1000);
  return churn(i);
}

function siteMixed(i) {
  const value = i % 97 === 0 ? `s${i}` : i;
  const name = first('mixed'); if (name) truth.blocking(name), Atomics.wait(cell, 0, 0, BLOCK_MS + 0 * `${value}`.length); else Atomics.wait(cell, 0, 0, BLOCK_MS + 0 * `${value}`.length);
  return churn(i);
}

const sites = [siteWait, siteNested, siteReviver, siteSort, siteFast, siteMixed];

function phase0(i) {
  return sites[i % sites.length](i);
}

function phase1(i) {
  return sites[i % sites.length](i);
}

function phase2(i) {
  return sites[i % sites.length](i);
}

function phase3(i) {
  return sites[i % sites.length](i);
}

const phases = [phase0, phase1, phase2, phase3];

function loop(deadline) {
  const start = Date.now();
  let ready = false;
  truth.report('phase', tid, phase, process.hrtime.bigint());
  for (let i = 0; Date.now() < deadline; i++) {
    const now = Math.floor((Date.now() - start) / PHASE_MS) % PHASES;
    if (now !== phase) {
      phase = now;
      truth.report('phase', tid, phase, process.hrtime.bigint());
    }
    phases[phase](i);
    if (!ready && seen.size === sites.length * PHASES) {
      ready = true;
      truth.report('ready', tid, process.hrtime.bigint());
    }
  }
  truth.report('done', tid, process.hrtime.bigint());
}

if (threads.isMainThread) {
  truth.report('waiting');
  fs.readSync(0, Buffer.alloc(1));
  for (let worker = 0; worker < Number(workers); worker++) {
    new threads.Worker(__filename, {workerData: [addonPath, fastPath, '0', seconds]});
  }
}
loop(Date.now() + Number(seconds) * 1000);
