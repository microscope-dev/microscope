// Variables microscope gives values through V8's inspector: each kind of
// binding a paused frame reaches, given each kind of value, which the
// program prints after the stop that gave them; a captured variable of an
// optimized frame; and the variables V8 does not assign, which the program
// prints unchanged. Run it with --allow-natives-syntax, so that hot() runs
// Turbofan code, as the program reports.

'use strict';

const util = require('node:util');
const truth = require('./truth.js');

let tally = 1;

// Closures that keep hot()'s context, so that it is allocated.
const readers = [];

function noop() {}

function outer(seed) {
  let captured = seed * 2;
  function inner(param) {
    let local = param + 1;
    const fixed = 'fixed';
    let big = 1n;
    let maybe = 5;
    let gone = 6;
    let holder = {a: 1};
    const other = {b: 2};
    {
      let blocked = 3;
      // A statement that loads no variable given a value: V8's register
      // optimizer may keep one loaded in the accumulator, whose old value
      // the statement at the pause then uses.
      noop(); // ASSIGN
      const shown = {param, local, fixed, big, maybe, gone, blocked, captured, tally};
      console.log(`ASSIGNED ${util.inspect(shown, {breakLength: Infinity})}`);
      console.log(`HOLDER ${holder === other} ${util.inspect(holder)}`);
    }
    return captured;
  }
  return inner(seed + 1);
}

function callee(n) {
  if (n === 99) {
    console.log(`HOT TIER ${truth.tierOf(hot)}`);
    return n; // CALLEE
  }
  return n;
}

function hot(n) {
  const kept = n * 3;
  let shared = n;
  readers.push(() => shared);
  const got = callee(n);
  return kept + got + shared;
}

function parks() {
  const waiting = 1;
  truth.checkpoint('parked');
  console.log(`PARKED waiting=${waiting} tally=${tally}`);
}

outer(10);

// eslint-disable-next-line no-new-func
const natives = new Function(
  'f',
  '%PrepareFunctionForOptimization(f); f(1); f(2); %OptimizeFunctionOnNextCall(f);',
);
natives(hot);
console.log(`HOT ${hot(99)}`);

parks();
console.log('ASSIGNMENTS DONE');
