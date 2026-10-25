'use strict';
const fs = require('node:fs');
const report = process.argv[2];
fs.writeFileSync(report, '');
process.on('SIGUSR1', () => fs.appendFileSync(report, 'SIGUSR1\n'));
process.stdin.once('data', () => setImmediate(() => process.exit(0)));
console.log('READY');
