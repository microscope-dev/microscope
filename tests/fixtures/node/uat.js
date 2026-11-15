


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

}
console.log('DONE');
