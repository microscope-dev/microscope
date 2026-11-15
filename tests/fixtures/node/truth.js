
















































































'use strict';

const fs = require('node:fs');
const util = require('node:util');
const threads = require('node:worker_threads');
const url = require('node:url');


const PARK_DEADLINE_MS = 30000;


const FUTEX = 202;


const PARKED = 0;
const RELEASED = 1;

const abort = threads.isMainThread && process.argv.slice(2).includes('--abort');

function escape(text) {
  return String(text).replace(/\\/g, '\\\\').replace(/\t/g, '\\t').replace(/\n/g, '\\n');
}

function line(fields) {
  return ['TRUTH', ...fields.map(escape)].join('\t') + '\n';
}



function write(text) {
  fs.writeSync(1, text);
}


function report(...fields) {
  write(line(fields));
}


function nativeId() {
  return Number(fs.readlinkSync('/proc/thread-self').split('/').pop());
}






function callSites(above) {
  return allSites(above).filter((site) => !site.isAsync());
}





function allSites(above) {
  const prepare = Error.prepareStackTrace;
  const limit = Error.stackTraceLimit;
  try {
    Error.prepareStackTrace = (_, sites) => sites;
    Error.stackTraceLimit = Infinity;
    const holder = {};
    Error.captureStackTrace(holder, above);
    return holder.stack;
  } finally {
    Error.prepareStackTrace = prepare;
    Error.stackTraceLimit = limit;
  }
}




const STATUS_BITS = {
  old: {executing: 11, turbofan: 12, ignition: 16, sparkplug: 17, maglev: 19},
  new: {executing: 10, turbofan: 11, ignition: 15, sparkplug: 16, maglev: 18},
};



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



const recorded = new Map();






function tier(fn) {
  if (natives === null) {
    return '-';
  }
  const now = tierOf(fn);
  recorded.set(callSites(tier).length - 1, {fn, entered: now});
  return now;
}


function tierAt(height, name) {
  const record = recorded.get(height);
  if (record === undefined || record.fn.name !== name) {
    return '-';
  }
  const now = tierOf(record.fn);
  return record.entered === now ? now : `${record.entered}>${now}`;
}


function describe(role, name, above, locals) {
  const id = nativeId();
  const all = allSites(above);
  const sites = all.filter((site) => !site.isAsync());
  const lines = [line(['thread', id, name, role])];
  const place = (site) => {
    const given = site.getFileName() ?? site.getScriptNameOrSourceURL() ?? '-';
    const file = given.startsWith('file:') ? url.fileURLToPath(given) : given;
    return [file, site.getLineNumber() ?? '-', site.getColumnNumber() ?? '-'];
  };
  sites.forEach((site, depth) => {
    const fn = site.getFunctionName() ?? '-';
    lines.push(line(['frame', id, depth, fn, ...place(site),
      tierAt(sites.length - 1 - depth, fn)]));
  });
  all.filter((site) => site.isAsync()).forEach((site, index) => {
    const combinator = site.isPromiseAll() || site.getPromiseIndex() !== null;
    lines.push(line(combinator
      ? ['async', id, index, `Promise.${site.getFunctionName()} (index ${site.getPromiseIndex()})`,
        '-', '-', '-']
      : ['async', id, index, site.getFunctionName() ?? '-', ...place(site)]));
  });
  for (const [local, value] of Object.entries(locals ?? {})) {
    lines.push(line(['value', id, 0, local, util.inspect(value, {customInspect: false}),
      util.inspect(value, {customInspect: false, compact: true, breakLength: Infinity}),
      util.types.isNativeError(value) ? 'error' : '-']));
  }
  recorded.clear();
  return {id, text: lines.join('')};
}



const workers = [];





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


  for (const {id} of reports) {
    while (!fs.readFileSync(`/proc/self/task/${id}/syscall`, 'utf8').startsWith(`${FUTEX} `)) {
      if (Date.now() > deadline) {
        throw new Error(`worker thread ${id} did not block`);
      }
    }
  }
  return reports.sort((a, b) => a.id - b.id);
}






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





function blocking(name) {
  write(line(['checkpoint', name]) + describe('blocked', name, blocking).text);
}


function release() {
  for (const worker of workers) {
    Atomics.store(worker.words, RELEASED, 1);
    Atomics.notify(worker.words, RELEASED);
  }
}

if (natives !== null) {

  for (const fn of [tier, tierOf, callSites, allSites, blocking, natives.status]) {
    natives.neverOptimize(fn);
  }


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

module.exports = {blocking, checkpoint, park, release, report, spawn, tier, tierOf};
