// Work a Node process's event loops wait on, and what Node itself says of
// it: a TCP server, timeouts, an interval, an unref'd timeout, a pending
// file read, a child process, a message port, a worker's own timer, and two
// thread-pool jobs, an open of the FIFO its first argument names, which
// blocks, and a long hash.
//
// Before its checkpoint the main thread prints, from async_hooks, which
// sees each resource Node creates as Node numbers it, one line for each
// live resource of a type Node keeps in an Environment's lists or its
// timer lists:
//
//     TRUTH  task  ASYNC-ID  TYPE  TRIGGER-ID  REFED  CALLBACK  LINE
//
// REFED is 1 or 0 where the fixture can ask (hasRef()), else -. CALLBACK
// is the name of the fixture's function Node will call for it, and LINE
// the line its source begins on in this file, or - for a resource whose
// callback is Node's own. For each libuv pool thread that is not waiting
// in a futex, as /proc says, it prints
//
//     TRUTH  pool  NATIVE-ID
//
// The worker reports its timer's async id as the value `timer` at park().

'use strict';

const asyncHooks = require('node:async_hooks');
const childProcess = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const net = require('node:net');
const threads = require('node:worker_threads');
const truth = require('./truth.js');

// The types an Environment keeps in its handle, request, or timer lists.
const LISTED = new Set(['TCPSERVERWRAP', 'PIPEWRAP', 'PROCESSWRAP', 'MESSAGEPORT',
  'FSREQCALLBACK', 'Timeout']);

// The futex system call, which an idle pool thread waits in.
const FUTEX = 202;

// How long the fixture waits for its pool jobs to start.
const POOL_DEADLINE_MS = 30000;

if (!threads.isMainThread) {
  const timer = setTimeout(function workerTimer() {}, 40000);
  truth.park({timer: +timer});
} else {
  const fifo = process.argv[2];
  if (fifo === undefined || fifo === '--abort') throw new Error('give the path of a FIFO');
  const live = new Map();
  let label = null;
  asyncHooks.createHook({
    init(id, type, trigger) {
      if (LISTED.has(type)) live.set(id, {type, trigger, label});
    },
    destroy(id) {
      live.delete(id);
    },
  }).enable();

  const named = (name, create) => {
    label = name;
    try {
      return create();
    } finally {
      label = null;
    }
  };

  const source = fs.readFileSync(__filename, 'utf8').split('\n');
  const lineOf = (name) => {
    const at = source.findIndex((text) => text.includes(`function ${name}(`));
    if (at < 0) throw new Error(`no function ${name} in the fixture`);
    return at + 1;
  };

  const server = named(null, () => net.createServer(function userConnection() {}));
  const timeout = named('sessionTimeout', () => setTimeout(function sessionTimeout() {}, 30000));
  const interval = named('poll', () => setInterval(function poll() {}, 5000));
  const idle = named('idleTimer', () => setTimeout(function idleTimer() {}, 60000).unref());
  // Opening a FIFO for reading blocks until a writer opens it, which none
  // does.
  named('configOpened', () => fs.open(fifo, 'r', function configOpened() {}));
  const child = named(null, () => childProcess.spawn(process.execPath,
    ['-e', 'process.stdin.resume()'], {stdio: ['pipe', 'ignore', 'ignore'], env: {}}));
  child.on('exit', function childExited() {});
  const {port1} = named(null, () => new threads.MessageChannel());
  port1.on('message', function onPortMessage() {});
  crypto.pbkdf2('p', 's', 2 ** 31 - 1, 64, 'sha512', function hashed() {});
  truth.spawn(__filename, 'ticker');

  const refs = new Map([[+timeout, timeout.hasRef()], [+interval, interval.hasRef()],
    [+idle, idle.hasRef()]]);

  // Busy pool threads: libuv names its threads, and one blocked anywhere
  // but a futex, or running, has a job.
  const busyPool = () => fs.readdirSync('/proc/self/task').filter((id) => {
    const name = fs.readFileSync(`/proc/self/task/${id}/comm`, 'utf8').trim();
    const call = fs.readFileSync(`/proc/self/task/${id}/syscall`, 'utf8');
    return name === 'libuv-worker' && !call.startsWith(`${FUTEX} `);
  });

  server.listen(0, '127.0.0.1', function listening() {
    refs.set(server._handle.getAsyncId(), server._handle.hasRef());
    const deadline = Date.now() + POOL_DEADLINE_MS;
    let busy = busyPool();
    while (busy.length < 2) {
      if (Date.now() > deadline) throw new Error(`the pool runs ${busy.length} jobs`);
      busy = busyPool();
    }
    for (const [id, {type, trigger, label: name}] of live) {
      const refed = refs.has(id) ? (refs.get(id) ? 1 : 0) : '-';
      truth.report('task', id, type, trigger, refed, name ?? '-', name ? lineOf(name) : '-');
    }
    for (const id of busy) truth.report('pool', id);
    truth.checkpoint('tasks');
    process.exit(0);
  });
}
