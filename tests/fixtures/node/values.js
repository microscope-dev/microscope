// A gallery of values of every kind F24's prototype rendered, each named at
// the checkpoint "gallery" in the frame of gallery(), with its canonical
// text (truth.js): numbers, BigInts, strings in each of V8's
// representations, objects and arrays in each of their element and
// property kinds, nested and cyclic values, collections, classes and their
// instances, getters, functions, errors, promises, typed arrays, and Buffers.
//
// util.inspect formats an error's stack, so the error whose stack nothing
// has read, unread, is named only at the next checkpoint, "unread", once a
// test has let the program go on from "gallery": what the program held at
// "gallery" is a stack of raw call sites, and its text at "unread" is the
// one Node formats from them.

'use strict';

const truth = require('./truth.js');

class Point {
  constructor(x, y) {
    this.x = x;
    this.y = y;
  }

  norm() {
    return Math.hypot(this.x, this.y);
  }
}

class Point3 extends Point {
  constructor(x, y, z) {
    super(x, y);
    this.z = z;
  }
}

// Strings built at run time, so that V8 keeps each in the representation
// its name says. Cons and sliced strings need 13 characters at least.
function strings() {
  const head = 'concatenated-';
  const long = 'a long one-byte string that a slice points into';
  const thin = ['thin-string', '-key'].join('');
  const keyed = {};
  // Using it as a key internalizes a copy and makes it a thin string.
  keyed[thin] = 1;
  return {
    oneByte: 'hello',
    twoByte: 'h\u00e9llo \u2603 \u{1f600}',
    cons: head + String(Date.UTC(2020, 0, 1)),
    sliced: long.substring(7, 30),
    thin,
    internalized: 'length',
    // Buffer#toString returns an external string past about 1 MB.
    external: Buffer.alloc(1100000, 0x78).toString('latin1'),
    empty: '',
    escaped: 'quote \' and "double" and \\ and \n',
  };
}

function objects() {
  const fast = {a: 1, b: 'two', c: [3]};
  const slow = {a: 1, b: 2, c: 3};
  delete slow.b;
  const dictionary = {};
  for (let i = 0; i < 2000; i++) {
    dictionary[`key${i}`] = i;
  }
  for (let i = 0; i < 1995; i++) {
    delete dictionary[`key${i}`];
  }
  const cyclic = {name: 'cyclic'};
  cyclic.self = cyclic;
  const getters = {
    get computed() {
      return 42;
    },
    set computed(value) {},
    get readOnly() {
      return 1;
    },
  };
  const nullPrototype = Object.assign(Object.create(null), {bare: true});
  const symbolKeyed = {[Symbol('tag')]: 'symbol value', plain: 1};
  return {
    fast, slow, dictionary, cyclic, getters, nullPrototype, symbolKeyed,
    nested: {level1: {level2: {level3: {level4: 'deep'}}}},
    empty: {},
  };
}

function arrays() {
  const holey = [1, , 3];
  const dictionary = [];
  dictionary[100000] = 'far';
  dictionary[0] = 'near';
  const cyclic = [1];
  cyclic.push(cyclic);
  return {
    packedSmi: [1, 2, 3],
    packedDouble: [1.5, 2.5, -0],
    packed: [1, 'two', {three: 3}],
    holey,
    holeyDouble: [1.5, , 3.5],
    dictionary,
    cyclic,
    long: Array.from({length: 150}, (_, i) => i),
    empty: [],
  };
}

function collections() {
  const map = new Map([['one', 1], [{key: true}, [2]]]);
  const set = new Set([1, 'two', 3n]);
  return {
    map,
    set,
    emptyMap: new Map(),
    weakMap: new WeakMap(),
    date: new Date(Date.UTC(2024, 1, 29, 12, 34, 56, 789)),
    invalidDate: new Date(NaN),
    regExp: /^(?<word>\w+)\s+\d*$/giu,
  };
}

function functions() {
  return {
    plain: function plain(a, b) {
      return a + b;
    },
    arrow: (x) => x,
    asyncFunction: async function fetchLater() {},
    generator: function* numbers() {
      yield 1;
    },
    classValue: Point,
    derived: Point3,
    bound: Point.prototype.norm.bind(new Point(3, 4)),
    anonymous: [function () {}][0],
    builtin: Math.max,
  };
}

function errors() {
  const read = new TypeError('read stack');
  void read.stack;
  const withCause = new Error('outer', {cause: new RangeError('inner')});
  void withCause.stack;
  return {read, withCause};
}

function promises() {
  const rejected = Promise.reject(new Error('handled'));
  rejected.catch(() => {});
  return {
    pending: new Promise(() => {}),
    fulfilled: Promise.resolve({ok: true}),
    rejected,
  };
}

function binary() {
  return {
    uint8: new Uint8Array([1, 2, 255]),
    int32: new Int32Array([-1, 0, 2147483647]),
    float64: new Float64Array([0.1, -0, NaN]),
    bigInt64: new BigInt64Array([1n, -1n]),
    view: new Uint16Array(new ArrayBuffer(16), 4, 3),
    arrayBuffer: new ArrayBuffer(4),
    buffer: Buffer.from('buffer bytes'),
    dataView: new DataView(new ArrayBuffer(2)),
  };
}

function gallery(unread) {
  const numbers = {
    smi: 42,
    negative: -7,
    heapNumber: 3.141592653589793,
    negativeZero: -0,
    notANumber: NaN,
    infinity: -Infinity,
    large: 1e21,
    small: 5e-324,
    unsafe: 2 ** 53 + 2,
  };
  const values = {
    numbers,
    bigints: {small: 1n, negative: -12345678901234567890n, huge: 2n ** 200n},
    primitives: {undefined, null: null, true: true, symbol: Symbol('described')},
    strings: strings(),
    objects: {...objects(), instance: new Point3(1, 2, 3)},
    arrays: arrays(),
    collections: collections(),
    functions: functions(),
    errors: errors(),
    promises: promises(),
    binary: binary(),
  };
  // A closure that captures values and unread makes V8 keep both in the
  // frame's context, where its scope names them at any stop. A variable
  // no closure captures lives in a register, which only V8's parser can
  // name (F11).
  const captured = () => [values, unread];
  truth.checkpoint('gallery', {locals: paths('values', values)});
  truth.checkpoint('unread', {locals: {unread}});
  return captured;
}

/**
 * Each value of each group of `values`, named by the expression that reaches
 * it from the variable `root`, as `values.strings.cons`.
 */
function paths(root, values) {
  return Object.fromEntries(Object.entries(values).flatMap(([group, members]) =>
    Object.entries(members).map(([name, value]) => [`${root}.${group}.${name}`, value])));
}

gallery(new SyntaxError('never read'));
