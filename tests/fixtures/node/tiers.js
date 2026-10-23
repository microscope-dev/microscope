// One stack with a frame of every tier V8 can put on it, each recorded by
// truth.tier() from inside the frame, for --allow-natives-syntax:
//
// turbofan > inlined > maglev > sparkplug > deoptimized > osr > ignition
//   > checkpoint("tiers")
//
// inlined runs inside turbofan's frame. deoptimized enters Turbofan code
// and leaves it at a map check the warm-up never failed; osr enters Maglev
// or Turbofan code on its loop. A tier this build cannot put on the stack
// is reported unavailable, with the reason, and its frame keeps the tier
// it has.
//
// Every call the optimized frames make is warmed with the same targets and
// argument types it makes for real, or the frame would deoptimize at it.
// Each call from one tier's function to the next goes through `chain` with
// a decoy target too, so that no compiler inlines its callee; only
// turbofan's call of inlined is monomorphic, to be inlined.

'use strict';

const truth = require('./truth.js');

// The most loop iterations osr runs waiting for its optimized code.
const OSR_ITERATIONS = 10_000_000;

function turbofan(real, chain) {
  truth.tier(turbofan);
  return inlined(real, chain) + 1;
}

function inlined(real, chain) {
  truth.tier(inlined);
  return chain[0](real, chain) + 1;
}

function maglev(real, chain) {
  truth.tier(maglev);
  return chain[1](real, chain) + 1;
}

function sparkplug(real, chain) {
  truth.tier(sparkplug);
  return chain[2](real, chain) + 1;
}

function deoptimized(real, chain) {
  truth.tier(deoptimized);
  const weight = chain[5].weight;
  return chain[3](real, chain) + weight;
}

function osr(real, chain) {
  if (!real) {
    return chain[4](real, chain);
  }
  truth.tier(osr);
  for (let i = 0; i < OSR_ITERATIONS; i++) {
    if (i === 2) {
      %OptimizeOsr();
    }
    const optimized = truth.tierOf(osr) !== 'ignition';
    const depth = chain[4](optimized, chain);
    if (optimized) {
      return depth;
    }
  }
  truth.report('unavailable', 'osr', `no optimized code after ${OSR_ITERATIONS} iterations`);
  return chain[4](true, chain);
}

function ignition(real) {
  truth.tier(ignition);
  if (real) {
    truth.checkpoint('tiers');
  }
  return 1;
}

function decoy() {
  return 1;
}

%NeverOptimizeFunction(ignition);
%NeverOptimizeFunction(decoy);
for (const fn of [turbofan, inlined, maglev, sparkplug, deoptimized, osr]) {
  %PrepareFunctionForOptimization(fn);
}

const chain = [maglev, sparkplug, deoptimized, osr, ignition, {weight: 1}];
const decoys = [decoy, decoy, decoy, decoy, decoy, {weight: 1}];
for (let i = 0; i < 3; i++) {
  turbofan(false, chain);
  turbofan(false, decoys);
  maglev(false, decoys);
  sparkplug(false, decoys);
  deoptimized(false, decoys);
}

%CompileBaseline(sparkplug);
if (%IsMaglevEnabled()) {
  %OptimizeMaglevOnNextCall(maglev);
} else {
  truth.report('unavailable', 'maglev', 'Maglev is not enabled in this build');
}
%OptimizeFunctionOnNextCall(deoptimized);
%OptimizeFunctionOnNextCall(turbofan);

// A weight of another shape, which deoptimized's code has never seen.
chain[5] = {heavy: true, weight: 2};
turbofan(true, chain);
