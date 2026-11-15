











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
