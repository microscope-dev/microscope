// Three JavaScript threads parked in nested functions of distinct kinds:
// the main thread in an arrow function under a class method, the worker
// alpha in a closure under a static method, and the worker beta in a
// function an async function called after its await resumed.
//
// main:  mainOuter > Ledger.settle > tally > checkpoint("parked")
// alpha: alphaStart > Sorter.sortAll > counterStep > park
// beta:  betaFetch (after await) > betaHandle > betaInner > park

'use strict';

const threads = require('node:worker_threads');
const truth = require('./truth.js');

class Ledger {
  constructor(entries) {
    this.entries = entries;
  }

  settle() {
    const tally = () => {
      const total = this.entries.reduce((sum, entry) => sum + entry, 0);
      truth.checkpoint('parked', {locals: {total}});
      return total;
    };
    return tally();
  }
}

function mainOuter() {
  const first = truth.spawn(__filename, 'alpha', {workerData: {role: 'alpha'}});
  const second = truth.spawn(__filename, 'beta', {workerData: {role: 'beta'}});
  const total = new Ledger([1, 2, 3]).settle();
  truth.release();
  return Promise.all([
    new Promise((resolve) => first.once('exit', resolve)),
    new Promise((resolve) => second.once('exit', resolve)),
  ]).then(() => total);
}

class Sorter {
  static sortAll(items) {
    let count = 0;
    const counterStep = () => {
      count += items.length;
      truth.park({count});
    };
    counterStep();
    return items.toSorted();
  }
}

function alphaStart() {
  return Sorter.sortAll([3, 1, 2]);
}

function betaInner(label) {
  truth.park({label});
}

function betaHandle(value) {
  betaInner(`beta-${value}`);
}

async function betaFetch() {
  const value = await Promise.resolve(7);
  betaHandle(value);
}

if (threads.isMainThread) {
  mainOuter();
} else if (threads.workerData.role === 'alpha') {
  alphaStart();
} else {
  betaFetch();
}
