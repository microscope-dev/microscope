// System calls made from JavaScript frames of each tier, for
// --allow-natives-syntax: each opens a path that names the call, and just
// before the open, on the same line, prints the stack V8's own call sites
// give for it:
//
//     TRUTH  call   PATH  NATIVE-ID  TIER
//     TRUTH  frame  PATH  DEPTH  FUNCTION  FILE  LINE
//
// TIER is the tier of the frame at DEPTH 0, which makes the open, as
// truth.tierOf() reads it: ignition, sparkplug, maglev, or turbofan.
// FUNCTION, FILE, and LINE are getFunctionName(), getFileName(), and
// getLineNumber(), or - where V8 has none. The frames of node:fs that run
// the open are above DEPTH 0 and not reported.
//
// The main thread opens /dev/full from a Turbofan frame, which a Sparkplug
// frame called, which an interpreted frame called, then /dev/zero from the
// Sparkplug frame and /dev/null from the interpreted one; then a worker
// opens /dev/urandom from an interpreted frame. Every function is warmed by
// opening /dev/random, which the tests ignore, so that the optimized code
// has seen the call it makes.
//
// With --hold, for the BPF engine, which reads V8 a moment after the
// program starts and the stacks a moment after each call:
//   - the TRUTH lines go to stderr, as the trace takes stdout;
//   - until stdin says "go", the program opens /dev/random from the
//     function probe every 20 ms, so that the test can see when the
//     engine names JavaScript frames;
//   - then it makes its calls, and the worker stays alive until stdin
//     ends, so that the engine reads its isolate while it lives.

'use strict';

const fs = require('node:fs');
const threads = require('node:worker_threads');
const truth = require('./truth.js');

// The path warm-up calls open, which no test looks at.
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

/**
 * Prints the stack of the caller of `flags`, which opens `path` on the
 * line that called it, with `fn`, the caller's function, for its tier; and
 * returns the flags the open takes.
 */
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
  // ignition > sparkplug > turbofan: each opens its own path after its
  // callee has, so the innermost, Turbofan's, comes first.
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
