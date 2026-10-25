// CPU time spent in JavaScript frames of each tier, for
// --allow-natives-syntax: the main thread spins in an interpreted frame,
// which calls a Sparkplug frame, which calls a Turbofan frame, each
// spinning a while itself before its call; a worker spins in an
// interpreted frame. Before spinning, each thread prints the stack V8's
// own call sites give for its innermost frame's call, and each frame's
// tier:
//
//     TRUTH  spin   NATIVE-ID  NAME
//     TRUTH  frame  NATIVE-ID  DEPTH  FUNCTION  FILE  LINE  TIER
//
// NAME is main or worker. DEPTH 0 is the innermost spinning frame, at the
// line it called tierOf() on; each outer frame's LINE is that of its call.
// TIER is truth.tierOf()'s, or - for a frame no tier is asked of. Each
// spinning function's loop is one line of its own, between its function
// line and its closing brace, so a sample in it is on a line within them.
// With --pool, a thread of libuv's pool derives a key with PBKDF2 for
// about as long, which runs no JavaScript, so it is in no isolate.
// The program spins for --seconds SECONDS, 3 by default, then exits. Run
// it with --sparkplug-filter=sparkplugSpin, which keeps V8 from compiling
// the interpreted frames' functions with Sparkplug as they grow hot.

'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const threads = require('node:worker_threads');
const truth = require('./truth.js');

const argv = process.argv.slice(2);
const seconds = Number(argv[argv.indexOf('--seconds') + 1]) || 3;

// How many rounds each frame spins between its calls: about a millisecond.
const ROUNDS = 200000;

function escape(text) {
  return String(text).replace(/\\/g, '\\\\').replace(/\t/g, '\\t').replace(/\n/g, '\\n');
}

function line(fields) {
  return ['TRUTH', ...fields.map(escape)].join('\t') + '\n';
}

function nativeId() {
  return Number(fs.readlinkSync('/proc/thread-self').split('/').pop());
}

/**
 * Prints, when `print` is set, the stack of the caller of `report`, named
 * `name`, with `tiers`. Warm-up calls it unprinted, so that optimized code
 * has seen the call.
 */
function report(print, name, tiers) {
  if (!print) {
    return;
  }
  const prepare = Error.prepareStackTrace;
  const limit = Error.stackTraceLimit;
  let sites;
  try {
    Error.prepareStackTrace = (_, given) => given;
    Error.stackTraceLimit = Infinity;
    const holder = {};
    Error.captureStackTrace(holder, report);
    sites = holder.stack;
  } finally {
    Error.prepareStackTrace = prepare;
    Error.stackTraceLimit = limit;
  }
  const id = nativeId();
  let text = line(['spin', id, name]);
  sites.forEach((site, depth) => {
    text += line(['frame', id, depth, site.getFunctionName() ?? '-',
      site.getFileName() ?? '-', site.getLineNumber() ?? '-', tiers[depth] ?? '-']);
  });
  fs.writeSync(1, text);
}

// Whether the next spin of the innermost frame reports its stack.
let reporting = false;

function turbofanSpin(acc) {
  for (let i = 0; i < ROUNDS; i++) acc = (acc + i * i) % 1000003;
  report(reporting, 'main', [truth.tierOf(turbofanSpin), truth.tierOf(sparkplugSpin),
    truth.tierOf(ignitionSpin)]);
  reporting = false;
  return acc;
}

function sparkplugSpin(acc) {
  for (let i = 0; i < ROUNDS; i++) acc = (acc + i * i) % 1000003;
  return turbofanSpin(acc);
}

function ignitionSpin(acc) {
  for (let i = 0; i < ROUNDS; i++) acc = (acc + i * i) % 1000003;
  return sparkplugSpin(acc);
}

function workerSpin(acc) {
  for (let i = 0; i < ROUNDS; i++) acc = (acc + i * i) % 1000003;
  report(reporting, 'worker', [truth.tierOf(workerSpin)]);
  reporting = false;
  return acc;
}

%NeverOptimizeFunction(report);
%NeverOptimizeFunction(ignitionSpin);
%NeverOptimizeFunction(sparkplugSpin);
%NeverOptimizeFunction(workerSpin);

function spinFor(spin, end) {
  let acc = 0;
  while (Date.now() < end) acc = spin(acc);
  return acc;
}

%NeverOptimizeFunction(spinFor);

function worker() {
  const end = Date.now() + seconds * 1000;
  reporting = true;
  return spinFor(workerSpin, end);
}

function main() {
  const end = Date.now() + seconds * 1000;
  %PrepareFunctionForOptimization(turbofanSpin);
  for (let i = 0; i < 3; i++) ignitionSpin(0);
  %CompileBaseline(sparkplugSpin);
  %OptimizeFunctionOnNextCall(turbofanSpin);
  ignitionSpin(0);
  reporting = true;
  if (argv.includes('--pool')) {
    // About a million rounds a second, past the spin's end.
    crypto.pbkdf2('secret', 'salt', (seconds + 1) * 1000000, 64, 'sha512', () => {});
  }
  new threads.Worker(__filename, {argv});
  return spinFor(ignitionSpin, end);
}

if (threads.isMainThread) {
  main();
} else {
  worker();
}
