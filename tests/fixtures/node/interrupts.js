// Interrupts through Node's inspector. The program first waits for a line
// in fs.readSync, where no JavaScript runs to pause, then spins in spin()
// until it ends, where an interrupt pauses the JavaScript running.
'use strict';
const fs = require('fs');

function spin(limit) {
  let total = 0;
  for (let i = 0; i < limit; i++) {
    total += i % 7; // SPIN
  }
  return total;
}

process.stdout.write('IDLE\n');
fs.readSync(0, Buffer.alloc(1));
process.stdout.write('SPINNING\n');
for (;;) {
  spin(100000);
}
