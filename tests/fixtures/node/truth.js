// What a Node fixture knows about itself, printed for microscope's tests.
//
// A fixture starts its worker threads with spawn(), and each worker parks
// with park(). The main thread then calls checkpoint(name), which prints
// TRUTH lines (tests/support/truth.rs reads them) from V8's own account of
// the process: every JavaScript thread's native id and its frames from V8's
// CallSite API, each frame's tier where the frame recorded one with tier(),
// and the values a fixture names in canonical text. Then it prints READY and
// parks until a line arrives on its standard input, so that a test can stop
// the process and compare what microscope reads with what the program said.
//
// Every line is a tab-separated record; a field escapes a backslash, tab, or
// newline as \\, \t, or \n:
//
//     TRUTH  process      PID  NODE-VERSION  V8-VERSION
//     TRUTH  checkpoint   NAME
//     TRUTH  thread       NATIVE-ID  NAME  ROLE
//     TRUTH  frame        NATIVE-ID  DEPTH  FUNCTION  FILE  LINE  COLUMN  TIER
//     TRUTH  value        NATIVE-ID  DEPTH  NAME  CANONICAL
//     TRUTH  unavailable  TIER  REASON
//
// NODE-VERSION is process.versions.node, as 26.12.1, and V8-VERSION
// process.versions.v8. NATIVE-ID is the thread's Linux thread id, from
// /proc/thread-self. NAME is main for the main thread and the name spawn()
// gave a worker. ROLE is printer for the thread that printed the checkpoint
// and parked for the others; the printer comes first, then the workers by
// native id.
//
// A thread's frames are the frames on its stack while it parks, innermost
// first, as CallSite reports them; V8's await chain, which is not on the
// stack, is left out. DEPTH 0 is the frame that called checkpoint() or
// park(). The frames inside this file and what it calls, above DEPTH 0, are
// not reported: the printer parks in fs.readSync (node:fs) under checkpoint(),
// a worker in Atomics.wait under park(), and a core's printer in
// process.abort() under checkpoint(). A test skips a thread's innermost frames
// up to the first one outside this file and node:fs. FUNCTION is
// getFunctionName(), FILE getFileName() or else getScriptNameOrSourceURL(),
// as the path url.fileURLToPath() gives where that is a file: URL, as an ES
// module's is, and LINE and COLUMN V8's, counted from 1; each is - when V8
// has none.
//
// TIER is the tier of the frame's code at the checkpoint, read from inside
// the process while every frame is on the stack, for a frame whose function
// called tier() in it after its last await: ignition, sparkplug, maglev, or
// turbofan for a frame of that tier's code, and inlined for a function that
// runs inside another's optimized frame, where no frame of its own exists.
// A frame whose tier changed after its tier() call shows both, FROM>TO:
// turbofan>ignition is a frame that deoptimized, and ignition>turbofan or
// ignition>maglev one that entered optimized code on a loop (OSR). Every
// other frame, and every frame when the process runs without
// --allow-natives-syntax, shows -. The tier is %GetOptimizationStatus()'s
// account of the topmost frame that runs the function, not of the code the
// function holds, which is what %ActiveTierIs*() and the status's other bits
// give and which differs after a deoptimization or OSR.
//
// A value is one a fixture names at checkpoint() or park(), and NAME how the
// frame at DEPTH 0 reaches it: a variable's name, or property accesses from
// one, as values.strings.cons. CANONICAL is
// util.inspect(value, {customInspect: false}), the text microscope must
// present the value as. An unavailable line says a tier fixture could not
// put a frame of TIER on the stack, and why.
//
// Only the standard library is used. Tests run fixtures with an empty
// environment, so no NODE_OPTIONS or user configuration changes them. Given
// --abort, checkpoint() aborts the process after READY instead of parking,
// to leave a core dump of the process as it reported it.

'use strict';

const fs = require('node:fs');
const util = require('node:util');
const threads = require('node:worker_threads');
const url = require('node:url');

// How long checkpoint() waits for every worker to park.
const PARK_DEADLINE_MS = 30000;

// The futex system call, which a thread blocked in Atomics.wait is in.
const FUTEX = 202;

// The shared words of a worker: whether it has parked, then whether it may go.
const PARKED = 0;
const RELEASED = 1;

const abort = threads.isMainThread && process.argv.slice(2).includes('--abort');

function escape(text) {
  return String(text).replace(/\\/g, '\\\\').replace(/\t/g, '\\t').replace(/\n/g, '\\n');
}

function line(fields) {
  return ['TRUTH', ...fields.map(escape)].join('\t') + '\n';
}

// Written straight to the file descriptor, so that nothing is buffered when
// the process stops or aborts.
function write(text) {
  fs.writeSync(1, text);
}

/** Prints one TRUTH line of fields, each escaped. */
function report(...fields) {
  write(line(fields));
}

/** The Linux thread id of the calling thread. */
function nativeId() {
  return Number(fs.readlinkSync('/proc/thread-self').split('/').pop());
}

/**
 * The call sites of the calling thread's stack, innermost first, from the
 * caller of `above` outward; the frames of `above` and what it called are
 * left out, and so is V8's await chain.
 */
function callSites(above) {
  const prepare = Error.prepareStackTrace;
  const limit = Error.stackTraceLimit;
  try {
    Error.prepareStackTrace = (_, sites) => sites;
    Error.stackTraceLimit = Infinity;
    const holder = {};
    Error.captureStackTrace(holder, above);
    return holder.stack.filter((site) => !site.isAsync());
  } finally {
    Error.prepareStackTrace = prepare;
    Error.stackTraceLimit = limit;
  }
}

// Each tier's topmost-frame bit in %GetOptimizationStatus(), and the bit
// that says some frame runs the function, by V8 line: 14.x dropped
// kAlwaysOptimize and moved every later bit down by one.
const STATUS_BITS = {
  old: {executing: 11, turbofan: 12, ignition: 16, sparkplug: 17, maglev: 19},
  new: {executing: 10, turbofan: 11, ignition: 15, sparkplug: 16, maglev: 18},
};

// The natives this library calls, or null when the process runs without
// --allow-natives-syntax.
const natives = (() => {
  let status;
  let neverOptimize;
  try {
    // eslint-disable-next-line no-new-func
    status = new Function('f', 'return %GetOptimizationStatus(f);');
    // eslint-disable-next-line no-new-func
    neverOptimize = new Function('f', '%NeverOptimizeFunction(f);');
  } catch {
    return null;
  }
  const major = Number(process.versions.v8.split('.')[0]);
  return {status, neverOptimize, bits: major >= 14 ? STATUS_BITS.new : STATUS_BITS.old};
})();

/**
 * The tier of the topmost frame that runs `fn`, as tier() reports it, or -
 * without natives. Called from inside `fn`, a function with no frame of its
 * own runs inlined in another's.
 */
function tierOf(fn) {
  if (natives === null) {
    return '-';
  }
  const status = natives.status(fn);
  const bit = (name) => (status & (1 << natives.bits[name])) !== 0;
  if (!bit('executing')) {
    return 'inlined';
  }
  for (const tier of ['turbofan', 'maglev', 'sparkplug', 'ignition']) {
    if (bit(tier)) {
      return tier;
    }
  }
  throw new Error(`no tier in ${fn.name}'s optimization status ${status.toString(2)}`);
}

// What tier() recorded on this thread, by the height of the frame that
// called it, counted from the outermost frame.
const recorded = new Map();

/**
 * Records that the frame calling it runs `fn`, whose tier the checkpoint
 * reports, and returns the frame's tier now. Call it after the frame's last
 * await, which moves the frame to another stack.
 */
function tier(fn) {
  if (natives === null) {
    return '-';
  }
  const now = tierOf(fn);
  recorded.set(callSites(tier).length - 1, {fn, entered: now});
  return now;
}

/** The TIER field of the frame at `height` whose function is `name`. */
function tierAt(height, name) {
  const record = recorded.get(height);
  if (record === undefined || record.fn.name !== name) {
    return '-';
  }
  const now = tierOf(record.fn);
  return record.entered === now ? now : `${record.entered}>${now}`;
}

/** The lines that describe the calling thread, from the caller of `above`. */
function describe(role, name, above, locals) {
  const id = nativeId();
  const sites = callSites(above);
  const lines = [line(['thread', id, name, role])];
  sites.forEach((site, depth) => {
    const given = site.getFileName() ?? site.getScriptNameOrSourceURL() ?? '-';
    const file = given.startsWith('file:') ? url.fileURLToPath(given) : given;
    const fn = site.getFunctionName() ?? '-';
    lines.push(line(['frame', id, depth, fn, file, site.getLineNumber() ?? '-',
      site.getColumnNumber() ?? '-', tierAt(sites.length - 1 - depth, fn)]));
  });
  for (const [local, value] of Object.entries(locals ?? {})) {
    lines.push(line(['value', id, 0, local, util.inspect(value, {customInspect: false}),
      util.inspect(value, {customInspect: false, compact: true, breakLength: Infinity}),
      util.types.isNativeError(value) ? 'error' : '-']));
  }
  recorded.clear();
  return {id, text: lines.join('')};
}

// The workers spawn() started, each with its shared words and the port it
// reports on.
const workers = [];

/**
 * Starts a worker thread named `name` that runs `file`, which parks with
 * park(); the checkpoint waits for it.
 */
function spawn(file, name, options = {}) {
  if (!threads.isMainThread) {
    throw new Error('only the main thread spawns workers');
  }
  const words = new Int32Array(new SharedArrayBuffer(8));
  const {port1, port2} = new threads.MessageChannel();
  const worker = new threads.Worker(file, {
    ...options,
    name,
    workerData: {...options.workerData, truth: {words, port: port2, name}},
    transferList: [...(options.transferList ?? []), port2],
  });
  workers.push({words, port: port1, name});
  return worker;
}

/**
 * Reports the calling worker's frames and the values of `locals` to the
 * main thread, then parks in Atomics.wait until the process ends.
 */
function park(locals) {
  const truth = threads.workerData?.truth;
  if (threads.isMainThread || truth === undefined) {
    throw new Error('park() runs on a worker spawn() started');
  }
  truth.port.postMessage(describe('parked', truth.name, park, locals));
  Atomics.store(truth.words, PARKED, 1);
  Atomics.notify(truth.words, PARKED);
  while (Atomics.load(truth.words, RELEASED) === 0) Atomics.wait(truth.words, RELEASED, 0);
}

/**
 * What each worker reported when it parked, by native id, once it is
 * blocked there.
 */
function parkedWorkers() {
  const deadline = Date.now() + PARK_DEADLINE_MS;
  const reports = workers.map((worker) => {
    while (Atomics.load(worker.words, PARKED) === 0) {
      if (Atomics.wait(worker.words, PARKED, 0, deadline - Date.now()) === 'timed-out') {
        throw new Error(`worker ${worker.name} did not park`);
      }
    }
    worker.parked ??= threads.receiveMessageOnPort(worker.port).message;
    return worker.parked;
  });
  // A worker that said it parked has reported every frame it parks in, but
  // only one blocked in the futex has a stack that stays still.
  for (const {id} of reports) {
    while (!fs.readFileSync(`/proc/self/task/${id}/syscall`, 'utf8').startsWith(`${FUTEX} `)) {
      if (Date.now() > deadline) {
        throw new Error(`worker thread ${id} did not block`);
      }
    }
  }
  return reports.sort((a, b) => a.id - b.id);
}

/**
 * Prints every thread's frames, and the values of `locals` in the caller's
 * frame; then parks until a line arrives on standard input, or parks for
 * good at its end; or, with --abort, aborts.
 */
function checkpoint(name, {locals} = {}) {
  if (!threads.isMainThread) {
    throw new Error('checkpoint() runs on the main thread');
  }
  const parked = parkedWorkers();
  const printer = describe('printer', 'main', checkpoint, locals);
  write(line(['checkpoint', name]) + printer.text + parked.map((w) => w.text).join('') +
    'READY\n');
  if (abort) {
    process.abort();
  }
  waitForLine();
}

/** Reads standard input until a newline, or parks for good at its end. */
function waitForLine() {
  const buffer = Buffer.alloc(1);
  for (;;) {
    if (fs.readSync(0, buffer, 0, 1, null) === 0) {
      const forever = new Int32Array(new SharedArrayBuffer(4));
      for (;;) Atomics.wait(forever, 0, 0);
    }
    if (buffer[0] === 0x0a) {
      return;
    }
  }
}

/** Lets every parked worker go on. */
function release() {
  for (const worker of workers) {
    Atomics.store(worker.words, RELEASED, 1);
    Atomics.notify(worker.words, RELEASED);
  }
}

if (natives !== null) {
  // The library's own frames stay out of the frames they measure.
  for (const fn of [tier, tierOf, callSites, natives.status]) natives.neverOptimize(fn);
  // A frame that runs bytecode must say so, or the status's layout is not
  // the one this library reads.
  const calibration = function calibration() {
    return tierOf(calibration);
  };
  natives.neverOptimize(calibration);
  if (calibration() !== 'ignition') {
    throw new Error(`V8 ${process.versions.v8}'s optimization status has an unknown layout`);
  }
}

if (threads.isMainThread) {
  report('process', process.pid, process.versions.node, process.versions.v8);
}

module.exports = {checkpoint, park, release, report, spawn, tier, tierOf};
