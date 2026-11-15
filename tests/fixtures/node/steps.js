


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


function countdown(n) {
  if (n === 0) return 0; // COUNTDOWN
  const rest = countdown(n - 1);
  return rest + n;
}


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



function relays() {
  const relayed = relay(listener, 2); // RELAYS
  return relayed;
}


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
