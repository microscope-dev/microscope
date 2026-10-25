// Threads that block in native code for about a millisecond at a time, at
// sites of known frames, as fast as they can, while V8 collects garbage,
// moves objects, and deoptimizes code under them, for live reads that must
// find each thread at one of its sites or leave it out
// (tests/node/live.rs).
//
// node --allow-natives-syntax [V8 flags] live-stress.js ADDON FAST WORKERS SECONDS
//
// ADDON and FAST are the built node-addon.node and the build's
// node-fast-ENTRY.node. The main thread and WORKERS workers each loop over
// the sites for SECONDS. Each prints, the first time it blocks at a site,
// the site's frames with truth.blocking('SITE TID'), on the line that
// blocks; then, once it has blocked at every site, `ready TID TIME`, and as
// it stops, `done TID TIME`, with TIME process.hrtime.bigint(). Between the
// two, the thread runs only these sites and the code that leads to them.
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

const tid = Number(fs.readlinkSync('/proc/thread-self').split('/').pop());
const cell = new Int32Array(new SharedArrayBuffer(4));
const seen = new Set();
let garbage = [];

/** Whether this is the thread's first block at `site`, which it reports. */
function first(site) {
  if (seen.has(site)) {
    return false;
  }
  seen.add(site);
  return true;
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
  if (first('wait')) truth.blocking(`wait ${tid}`), Atomics.wait(cell, 0, 0, BLOCK_MS); else Atomics.wait(cell, 0, 0, BLOCK_MS);
  return churn(i);
}

function nestedInner(i) {
  if (first('nested')) truth.blocking(`nested ${tid}`), Atomics.wait(cell, 0, 0, BLOCK_MS); else Atomics.wait(cell, 0, 0, BLOCK_MS);
  return churn(i);
}

function siteNested(i) {
  addon.inspect({}, 'text', function nestedCallback() {
    nestedInner(i);
  });
  return churn(i);
}

function reviverInner(i) {
  if (first('reviver')) truth.blocking(`reviver ${tid}`), Atomics.wait(cell, 0, 0, BLOCK_MS); else Atomics.wait(cell, 0, 0, BLOCK_MS);
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
  if (first('sort')) truth.blocking(`sort ${tid}`), Atomics.wait(cell, 0, 0, BLOCK_MS); else Atomics.wait(cell, 0, 0, BLOCK_MS);
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
  if (first('fast')) truth.blocking(`fast ${tid}`), fast.nap(BLOCK_MS * 1000); else fast.nap(BLOCK_MS * 1000);
  return churn(i);
}

function siteMixed(i) {
  const value = i % 97 === 0 ? `s${i}` : i;
  if (first('mixed')) truth.blocking(`mixed ${tid}`), Atomics.wait(cell, 0, 0, BLOCK_MS + 0 * `${value}`.length); else Atomics.wait(cell, 0, 0, BLOCK_MS + 0 * `${value}`.length);
  return churn(i);
}

const sites = [siteWait, siteNested, siteReviver, siteSort, siteFast, siteMixed];

function loop(deadline) {
  for (let i = 0; Date.now() < deadline; i++) {
    sites[i % sites.length](i);
    if (i === sites.length - 1) {
      truth.report('ready', tid, process.hrtime.bigint());
    }
  }
  truth.report('done', tid, process.hrtime.bigint());
}

if (threads.isMainThread) {
  for (let worker = 0; worker < Number(workers); worker++) {
    new threads.Worker(__filename, {workerData: [addonPath, fastPath, '0', seconds]});
  }
}
loop(Date.now() + Number(seconds) * 1000);
