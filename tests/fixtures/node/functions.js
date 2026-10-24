// Functions that `js:NAME` breakpoints stop in: a declaration, an arrow
// held in a constant, a method, one in a required module, and one nested in
// a function, which V8 compiles only once its parent first runs. The main
// script runs them all, then waits for a line, then runs the nested one
// again.
'use strict';
const { helper } = require('./functions_lib.js');

function norm(p) {
  return Math.sqrt(p.x * p.x + p.y * p.y); // NORM
}

const handler = (n) => n + 1; // HANDLER

class Shape {
  area() {
    return 6; // AREA
  }
}

function outer() {
  function inner() {
    return 2; // INNER
  }
  return inner();
}

console.log('FIRST', outer(), norm({ x: 3, y: 4 }), handler(1), new Shape().area(), helper(2));
process.stdin.once('data', () => {
  process.stdin.destroy();
  console.log('LATER', outer());
});
