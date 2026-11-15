























'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const threads = require('node:worker_threads');
const truth = require('./truth.js');

const argv = process.argv.slice(2);
const seconds = Number(argv[argv.indexOf('--seconds') + 1]) || 3;
const depth = argv.includes('--depth') ? Number(argv[argv.indexOf('--depth') + 1]) : 0;


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


function nest(levels, fn) {
  return levels > 0 ? nest(levels - 1, fn) : fn();
}

%NeverOptimizeFunction(nest);

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

    crypto.pbkdf2('secret', 'salt', (seconds + 1) * 1000000, 64, 'sha512', () => {});
  }
  new threads.Worker(__filename, {argv});
  return depth > 0 ? nest(depth, () => spinFor(ignitionSpin, end)) : spinFor(ignitionSpin, end);
}

if (threads.isMainThread) {
  main();
} else {
  worker();
}
