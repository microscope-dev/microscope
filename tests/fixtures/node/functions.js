




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
