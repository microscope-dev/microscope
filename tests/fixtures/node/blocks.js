





'use strict';

function noop() {}

function g() {
  let total = 0;
  for (let r = 0; r < 3; r++) {
    let c = 10 * r;
    noop(); // BLOCK
    c += 1;
    total += c;
  }
  console.log(`TOTAL ${total}`);
  return total;
}




function deep(n) {
  return n === 0 ? g() : deep(n - 1);
}

deep(4);
const text = JSON.stringify({list: [1, 2, 3], nested: {deep: [{a: 1}]}});
const parsed = JSON.parse(text);
const formatted = new Intl.NumberFormat('en-US').format(1234567.891);
const shape = {x: 1};
shape.x = 1.5;
shape.x = {parsed};
console.log(`NATIVE ${text.length} ${formatted} ${typeof shape.x}`);
console.log('BLOCKS DONE');
