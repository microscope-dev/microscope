// After fs.readSync, stay inside spin() so every JavaScript pause has that frame.
// Keep the accumulator bounded during long stress runs.

'use strict';
const fs = require('fs');

function spin(limit) {
  let total = 0;
  for (let i = 0; ; i = (i + 1) % limit) {
    total = (total + i % 7) % limit; // SPIN
  }
}

process.stdout.write('IDLE\n');
fs.readSync(0, Buffer.alloc(1));
process.stdout.write('SPINNING\n');
spin(100000);
