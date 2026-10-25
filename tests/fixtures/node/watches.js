// Variables microscope watches in V8's frames: an interpreter register
// and an argument, whose watches end with the call, before the next call
// builds its frame in the same place; a captured variable in a context
// that collections move, young and old, each of whose stores is reported
// once and nothing else; frames V8 replaces on a loop, from Sparkplug's
// code and from the interpreter's; and a variable V8 14.6 keeps in a
// context cell, which is refused. The program prints what it stores. Run
// it with --expose-gc and --stress-compaction, so that each full
// collection moves the context.

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
  // Four slots, so that V8 14.6 keeps no context cells in the context.
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

// Back to back, so that the second call's frame is the first write in
// the first's place.
const first = counter(3);
const second = counter(4);
console.log(`COUNTER ${first}`);
console.log(`COUNTER ${second}`);
moved();
celled();
// An allocating loop, which V8 replaces while the interpreter runs it,
// from the bytecode handler's frame, before Sparkplug compiles it.
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
