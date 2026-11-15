


'use strict';
const fs = require('fs');

function parse(text) {
  return JSON.parse(text); // CAUGHT_THROW
}

function check(value) {
  if (value > 2) throw new RangeError(`too big: ${value}`); // UNCAUGHT_THROW
  return value;
}

async function rejects() {
  try {
    await Promise.reject(new TypeError('rejected')); // REJECTS
  } catch {
    return 'handled';
  }
}

let parsed;
try {
  parsed = parse('{oops');
} catch (error) {
  parsed = error.name;
}

const exists = fs.existsSync('/no/such/file');
console.log(`parsed ${parsed} exists ${exists}`);
rejects().then((outcome) => console.log(`rejection ${outcome}`));
check(1);
check(3);
