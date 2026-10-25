// Async functions resumed after an await, each with a chain of callers
// suspended in await that V8 reports as the async frames of its stack
// trace, one checkpoint for each way a chain goes on:
//
// chain:       inner < middle < Promise.all (index 1) < outer < run
// resumed:     middle < Promise.all (index 1) < outer < run
// allSettled:  settledLeaf < Promise.allSettled (index 0) < viaAllSettled < run
// any:         anyLeaf < Promise.any (index 0) < viaAny < run
// then:        thenLeaf < (a then's promise) < viaThen < run
// resolver:    resolverLeaf < (a new Promise's resolve) < viaResolver < run
// generator:   produce, an async generator < consume, by for await < run
// shared:      sharedLeaf, whose promise two functions await, which ends
//              V8's chain
//
// The worker parks in workerLeaf, resumed after an await, which
// workerOuter awaits, for every checkpoint. middle keeps `captured` in
// its context, and `plain`, which no closure uses, in a register: the
// one its generator saved at chain, and its frame holds at resumed.

'use strict';

const threads = require('node:worker_threads');
const truth = require('./truth.js');

const tick = () => new Promise((resolve) => setImmediate(resolve));

async function settleLater(value) {
  await tick();
  return value;
}

async function inner(x) {
  const local = {inner: x};
  await tick();
  truth.checkpoint('chain', {locals: {local}});
  return local;
}

async function middle(y) {
  const captured = y * 2;
  const plain = `plain-${y / 2}`;
  const result = await inner(y + 1);
  truth.checkpoint('resumed');
  const joined = `${plain} ${result.inner}`;
  return () => `${captured} ${joined}`;
}

async function outer() {
  const [, describe] = await Promise.all([settleLater(1), middle(20)]);
  return describe();
}

async function settledLeaf() {
  await tick();
  truth.checkpoint('allSettled');
}

async function viaAllSettled() {
  return await Promise.allSettled([settledLeaf(), settleLater(2)]);
}

async function anyLeaf() {
  await tick();
  truth.checkpoint('any');
}

async function viaAny() {
  return await Promise.any([anyLeaf()]);
}

async function thenLeaf() {
  await tick();
  truth.checkpoint('then');
  return 3;
}

async function viaThen() {
  return await thenLeaf().then((value) => value + 1);
}

async function resolverLeaf() {
  await tick();
  truth.checkpoint('resolver');
  return 5;
}

async function viaResolver() {
  return await new Promise((resolve) => {
    resolverLeaf().then(resolve);
  });
}

async function* produce() {
  await tick();
  truth.checkpoint('generator');
  yield 6;
}

async function consume() {
  for await (const value of produce()) {
    return value;
  }
  return undefined;
}

async function sharedLeaf() {
  await tick();
  truth.checkpoint('shared');
}

async function waitOn(promise) {
  await promise;
}

async function viaShared() {
  const shared = sharedLeaf();
  await Promise.all([waitOn(shared), waitOn(shared)]);
}

async function run() {
  const worker = truth.spawn(__filename, 'worker');
  await outer();
  await viaAllSettled();
  await viaAny();
  await viaThen();
  await viaResolver();
  await consume();
  await viaShared();
  truth.release();
  await new Promise((resolve) => worker.once('exit', resolve));
}

async function workerLeaf() {
  await tick();
  truth.park({});
}

async function workerOuter() {
  await workerLeaf();
}

if (threads.isMainThread) {
  run();
} else {
  workerOuter();
}
