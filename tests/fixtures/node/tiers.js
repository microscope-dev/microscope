

















'use strict';

const truth = require('./truth.js');


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
    if (optimized) {

      %ClearFunctionFeedback(osr);
    }
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


chain[5] = {heavy: true, weight: 2};
turbofan(true, chain);
