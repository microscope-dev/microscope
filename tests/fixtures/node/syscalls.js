





























'use strict';

const fs = require('node:fs');
const threads = require('node:worker_threads');
const truth = require('./truth.js');


const WARM = '/dev/random';

const HOLD = process.argv.slice(2).includes('--hold');

function escape(text) {
  return String(text).replace(/\\/g, '\\\\').replace(/\t/g, '\\t').replace(/\n/g, '\\n');
}

function line(fields) {
  return ['TRUTH', ...fields.map(escape)].join('\t') + '\n';
}

function nativeId() {
  return Number(fs.readlinkSync('/proc/thread-self').split('/').pop());
}






function flags(path, fn) {
  if (path === WARM) {
    return 'r';
  }
  const prepare = Error.prepareStackTrace;
  const limit = Error.stackTraceLimit;
  let sites;
  try {
    Error.prepareStackTrace = (_, given) => given;
    Error.stackTraceLimit = Infinity;
    const holder = {};
    Error.captureStackTrace(holder, flags);
    sites = holder.stack;
  } finally {
    Error.prepareStackTrace = prepare;
    Error.stackTraceLimit = limit;
  }
  let text = line(['call', path, nativeId(), truth.tierOf(fn)]);
  sites.forEach((site, depth) => {
    text += line(['frame', path, depth, site.getFunctionName() ?? '-',
      site.getFileName() ?? '-', site.getLineNumber() ?? '-']);
  });
  fs.writeSync(HOLD ? 2 : 1, text);
  return 'r';
}

function ignitionOpens(path, chain) {
  if (chain.length > 0) {
    chain[0](path, chain.slice(1));
  }
  fs.closeSync(fs.openSync(path, flags(path, ignitionOpens))); // IGNITION
}

function sparkplugOpens(path, chain) {
  if (chain.length > 0) {
    chain[0](path, chain.slice(1));
  }
  fs.closeSync(fs.openSync(path, flags(path, sparkplugOpens))); // SPARKPLUG
}

function turbofanOpens(path, chain) {
  if (chain.length > 0) {
    chain[0](path, chain.slice(1));
  }
  fs.closeSync(fs.openSync(path, flags(path, turbofanOpens))); // TURBOFAN
}

function worker() {
  ignitionOpens('/dev/urandom', []); // WORKER
  if (HOLD) {
    threads.parentPort.once('message', () => threads.parentPort.close());
  }
}

%NeverOptimizeFunction(flags);
%NeverOptimizeFunction(ignitionOpens);
%NeverOptimizeFunction(worker);

function main() {
  %PrepareFunctionForOptimization(sparkplugOpens);
  %PrepareFunctionForOptimization(turbofanOpens);
  for (let i = 0; i < 3; i++) {
    turbofanOpens(WARM, []);
    sparkplugOpens(WARM, []);
    ignitionOpens(WARM, [(p, c) => sparkplugOpens(WARM, [(q, d) => turbofanOpens(WARM, d)])]);
  }
  %CompileBaseline(sparkplugOpens);
  %OptimizeFunctionOnNextCall(turbofanOpens);


  ignitionOpens('/dev/null', [(p, c) => sparkplugOpens('/dev/zero',
    [(q, d) => turbofanOpens('/dev/full', d)])]); // MAIN
  return new threads.Worker(__filename, {argv: process.argv.slice(2)});
}

if (!threads.isMainThread) {
  worker();
} else if (HOLD) {
  const probe = setInterval(function probe() {
    fs.closeSync(fs.openSync(WARM, 'r'));
  }, 20);
  let input = '';
  let started = null;
  process.stdin.setEncoding('utf8');
  process.stdin.on('data', (chunk) => {
    input += chunk;
    if (started === null && input.includes('go\n')) {
      clearInterval(probe);
      started = main();
    }
  });
  process.stdin.on('end', () => {
    clearInterval(probe);
    started?.postMessage('end');
  });
} else {
  main();
}
