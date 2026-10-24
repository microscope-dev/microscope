// What the editors' acceptance tests and the DAP tests debug: a line to
// break at and step from, and an exception the program throws and catches
// itself.
'use strict';
function area(side) {
  const result = side * side; // AREA_LINE
  return result; // RETURN_LINE
}

function fail() {
  throw new RangeError('the shape is wrong'); // THROW_LINE
}

console.log('AREA', area(4));
try {
  fail();
} catch {
  // The program expects this.
}
console.log('DONE');
