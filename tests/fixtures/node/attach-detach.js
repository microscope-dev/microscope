'use strict';
const fs = require('node:fs');
const inspector = require('node:inspector');
const url = inspector.url;
const report = process.argv[2];
Object.defineProperty(inspector, 'url', {get() { throw new Error('user url getter'); }});
Object.defineProperty(inspector, 'close', {get() { throw new Error('user close getter'); }});
function reached(n) {
  return n + 1;
}
let before;
process.stdin.on('data', () => {
  if (before === undefined) {
    before = url() ?? null;
    fs.writeFileSync(report, JSON.stringify({before}));
    reached(41);
  } else {
    fs.writeFileSync(report, JSON.stringify({before, after: url() ?? null}));
    process.exit(0);
  }
});
console.log('READY');
