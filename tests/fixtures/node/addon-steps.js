// Steps into a Node-API addon's C functions and back: adds() calls one
// that calls no JavaScript, and callsBack() one that calls cb() before it
// returns. The addon is the one build/test-programs holds.

'use strict';

const path = require('node:path');
const addon = require(path.join(__dirname, '../../../build/test-programs/node-addon.node'));

function cb() {
  const inner = 1;
  return inner + 1;
}

function adds() {
  const sum = addon.add(1, 2); // CALLS_ADD
  const after = sum + 1;
  return after;
}

function callsBack() {
  const object = {a: 1};
  const got = addon.inspect(object, 'hello', cb); // CALLS_INSPECT
  const after = got + 1;
  return after;
}

console.log(`ADDS ${adds()}`);
console.log(`CALLS BACK ${callsBack()}`);
console.log('ADDON STEPS DONE');
