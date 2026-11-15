









'use strict';

function noop() {}

function counter(rounds) {
  let total = 0;
  noop(); // COUNTER
  for (let i = 0; i < rounds; i++) {
    total += i + 1;
  }
  return total;
}

function moved() {

  let a = null;
  let b = 0;
  let c = 0;
  let d = 0;
  const read = () => [a, b, c, d];
  noop(); // MOVED
  for (let round = 1; round <= 3; round++) {
    gc({type: 'minor'});
    gc();
    a = {round};
    console.log(`MOVED ${round}`);
  }
  noop(); // MOVED_END
  return read;
}

function celled() {
  let x = 0;
  const read = () => x;
  noop(); // CELLED
  x = 1;
  console.log(`CELLED ${x}`);
  return read;
}

function hot(n) {
  let sum = 0;
  noop(); // HOT
  for (let i = 0; i < n; i++) {
    sum += i & 7;
  }
  return sum;
}



const first = counter(3);
const second = counter(4);
console.log(`COUNTER ${first}`);
console.log(`COUNTER ${second}`);
moved();
celled();


function spin(n) {
  const start = 7;
  noop(); // SPIN
  let keep = null;
  for (let i = 0; i < n; i++) {
    keep = {i, s: 'x' + i};
  }
  return start + keep.i;
}

console.log(`HOT ${hot(20000000)}`);
console.log(`SPIN ${spin(1000000)}`);
noop(); // DONE
console.log('WATCHES DONE');
