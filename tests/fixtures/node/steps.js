// Steps through Node's inspector. Each function is a case the step tests
// drive from a breakpoint on its marked line; cdp_oracle.js runs the same
// steps under Node's own inspector, which says where each one ends.
'use strict';
const { EventEmitter } = require('events');
const path = require('path');
const { relay } = require('./vendor/relay');

function leaf(x) {
  const doubled = x * 2; // LEAF
  return doubled;
}

function calls(n) {
  const a = leaf(n); // CALLS
  const b = a + 1;
  return b;
}

// Recursion: a step over the inner call stops in this activation.
function countdown(n) {
  if (n === 0) return 0; // COUNTDOWN
  const rest = countdown(n - 1);
  return rest + n;
}

// A listener Node's own emit calls: a step into emit reaches it.
function listener(value) {
  return value + 1; // LISTENER
}

function emits() {
  const emitter = new EventEmitter();
  emitter.on('tick', listener);
  emitter.emit('tick', 1); // EMITS
  const joined = path.join('a', 'b'); // JOINS
  return joined;
}

// A library calling back: a step into relay reaches the listener when
// the library is skipped.
function relays() {
  const relayed = relay(listener, 2); // RELAYS
  return relayed;
}

// A builtin calling back: a step into map reaches the callback.
function maps() {
  const values = [1, 2].map((value) => value * 3); // MAPS
  return values;
}

async function later() {
  await null;
  return 7;
}

async function awaits() {
  const value = await later(); // AWAITS
  return value + 1;
}

calls(5);
countdown(3);
emits();
maps();
relays();
awaits().then((value) => console.log(`awaited ${value}`));
