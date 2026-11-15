


'use strict';
const net = require('net');
const { Worker, isMainThread, workerData } = require('worker_threads');

function served(n) {
  const reply = n + 1; // SERVED
  return reply;
}

if (isMainThread) {
  const server = net.createServer().listen(0, '127.0.0.1', () => {
    process.stdout.write('READY\n');
    process.stdin.once('data', () => {
      process.stdin.destroy();
      console.log('MAIN', served(41));
      const worker = new Worker(__filename, { workerData: 7 });
      worker.on('exit', () => server.close());
    });
  });
} else {
  console.log('WORKER', served(workerData));
}
