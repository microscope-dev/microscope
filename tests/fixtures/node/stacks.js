// Three JavaScript threads parked in nested functions of distinct kinds:
// the main thread in an arrow function under a class method, the worker
// alpha in a closure under a static method, and the worker beta in a
// function an async function called after its await resumed.
//
// main:  mainOuter > Ledger.settle > tally > checkpoint("parked")
// alpha: alphaStart > Sorter.sortAll > counterStep > park
// beta:  betaFetched (after await) > betaHandled > betaInnermost > park
//
// V8 names a function whose map no longer keeps the `name` accessor every
// function starts with by its `name` data property, its own or else its
// prototypes': betaHandle's and betaInner's are redefined as data, which
// makes their maps dictionaries, and betaFetch has none of its own but a
// prototype that keeps one among its many properties.

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

// Deleting a property other than the last makes an object's map a
// dictionary.
function toDictionary(object) {
  for (let index = 0; index < 32; index++) {
    object[`property${index}`] = index;
  }
  delete object.property0;
}

Object.defineProperty(betaHandle, 'name', {value: 'betaHandled'});
toDictionary(betaInner);
Object.defineProperty(betaInner, 'name', {value: 'betaInnermost'});

const named = Object.create(Object.getPrototypeOf(betaFetch));
for (let index = 0; index < 16; index++) {
  named[`property${index}`] = index;
}
Object.defineProperty(named, 'name', {value: 'betaFetched'});
delete betaFetch.name;
Object.setPrototypeOf(betaFetch, named);

if (threads.isMainThread) {
  mainOuter();
} else if (threads.workerData.role === 'alpha') {
  alphaStart();
} else {
  betaFetch();
}
