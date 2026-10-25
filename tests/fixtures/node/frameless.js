// JavaScript that calls a Node-API addon whose code runs with rbp holding
// no frame pointer (frameless.c), so that a walk of the frame-pointer chain
// from inside it reads none of the JavaScript frames that called it. The
// first argument is the path of the built addon; then one of:
//
//   --opens          every 10 ms until standard input ends, probe() opens
//                    /dev/random through node:fs, whose frames a walk
//                    follows, then framelessOpens() opens /dev/zero
//                    through the addon
//   --spin SECONDS   framelessSpins() spins in the addon for SECONDS, a few
//                    milliseconds a call

'use strict';

const fs = require('node:fs');
const addon = require(process.argv[2]);

const [mode, given] = process.argv.slice(3);
const end = Date.now() + Number(given) * 1000;
let ended = false;

function probe() {
  fs.closeSync(fs.openSync('/dev/random', 'r'));
}

function framelessOpens() {
  const fd = addon.open('/dev/zero');
  if (fd >= 0) {
    fs.closeSync(fd);
  }
}

function framelessSpins() {
  while (Date.now() < end) {
    addon.spin(1000000);
  }
}

function tick() {
  probe();
  framelessOpens();
  if (!ended) {
    setTimeout(tick, 10);
  }
}

if (mode === '--opens') {
  process.stdin.on('end', () => {
    ended = true;
  });
  process.stdin.resume();
  tick();
} else if (mode === '--spin') {
  framelessSpins();
} else {
  throw new Error('frameless.js ADDON --opens | --spin SECONDS');
}
