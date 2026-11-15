


'use strict';
function area(width, height) {
  const product = width * height; // AREA_LINE
  return product; // RETURN_LINE
}
let total = 0;
for (let index = 0; index < 6; index++) {
  const step = area(index, 2);
  total += step; // LOOP_LINE
}
console.log(`total ${total}`);
