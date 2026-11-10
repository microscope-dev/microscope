// Closes Node's inspector once it reads a line, then runs a function a
// breakpoint waits in, and stays up to be paused.
'use strict';
const inspector = require('inspector');

function later() {
  return 1; // LATER
}

console.log('READY');
process.stdin.once('data', () => {
  process.stdin.destroy();
  inspector.close();
  console.log('CLOSED', later());
  setInterval(() => {}, 1000);
});
