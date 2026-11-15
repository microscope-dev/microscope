

'use strict';
const { halt } = require('./vendor/halt');

function first(n) {
  const doubled = n * 2;
  debugger; // FIRST
  return doubled;
}

function looped(count) {
  let total = 0;
  for (let i = 0; i < count; i++) {
    debugger; // LOOPED
    total += i;
  }
  return total;
}

first(3);
halt();
looped(2);
console.log('DONE');
