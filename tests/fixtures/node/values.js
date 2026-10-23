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
  // No prototype names a constructor, so inspect names each object as V8
  // does and ends at a prototype whose own prototype is null.
  function Orphan() {}
  Orphan.prototype = Object.create(null);
  // Through 13.6 V8 names an instance by its constructor's shared name,
  // 14.6 by its `name` property: `Renamed`, and `Object` for an empty one.
  function Named() {}
  Named.prototype = Object.create(null);
  Object.defineProperty(Named, 'name', {value: 'Renamed'});
  function Nameless() {}
  Nameless.prototype = Object.create(null);
  Object.defineProperty(Nameless, 'name', {value: ''});
  class Classless {
    constructor() {
      this.k = 1;
    }
  }
  Object.setPrototypeOf(Classless.prototype, null);
  delete Classless.prototype.constructor;
  const tagged = Object.create(Object.create(null), {
    [Symbol.toStringTag]: {value: 'Tagged'},
  });
  return {
    fast, slow, dictionary, cyclic, getters, nullPrototype, symbolKeyed,
    nested: {level1: {level2: {level3: {level4: 'deep'}}}},
    empty: {},
    unnamedChain: Object.create(Object.create(null)),
    unnamedChains: Object.create(Object.create(Object.create(null))),
    unnamedKeyedEnd: Object.create(Object.assign(Object.create(null), {end: 1})),
    unnamedInstance: new Orphan(),
    unnamedRenamed: new Named(),
    unnamedNameless: new Nameless(),
    unnamedClass: new Classless(),
    unnamedTagged: Object.create(tagged),
    unnamedDeep: {a: {b: {c: Object.create(Object.create(Object.create(null)))}}},
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

// The errors of unread, none of whose stacks anything reads: each is made
// where V8 writes a call site its own way, so that inspecting unread at
// "unread" formats every one.

class Widget {}

function describeWidget() {
  return new Error('a method under another name');
}

class Gadget {
  constructor() {
    this.error = new Error('a constructor');
  }
}

class Fielded {
  error = new Error('a field initializer');
}

class Shape {
  static create() {
    return new Error('a static method');
  }

  area() {
    return new Error('an instance method');
  }
}

// A plain function's own method. V8 12.4 names the receiver's type by its
// constructor, Function, and 13.6 by the function itself.
function Registry() {}
Registry.lookup = function lookup() {
  return new Error("a function's method");
};

const withGetter = {
  get broken() {
    return new Error('a getter');
  },
};

function mapOne() {
  return [1].map(function mapped() {
    return new Error('a callback under a builtin');
  })[0];
}

function evaluate() {
  return eval("new Error('an eval')");
}

// Scripts given line and column offsets, one named by its sourceURL, whose
// positions V8 counts from the comment's script rather than the offsets.
const vm = require('node:vm');
const fromUrl = vm.runInThisContext(
  "(function fromUrl() { return new Error('a sourceURL'); })\n//# sourceURL=named-by-url.js",
  {filename: 'offset-script.js', lineOffset: 10, columnOffset: 4});
const fromOffset = vm.runInThisContext(
  "(function fromOffset() { return new Error('a script with offsets'); })",
  {filename: 'offset-script.js', lineOffset: 10, columnOffset: 4});

async function settle() {
  await null;
  return new Error('after an await');
}

async function awaitSettle() {
  return await settle();
}

async function second() {
  await null;
  return new Error('under Promise.all');
}

async function gatherAll() {
  const [, error] = await Promise.all([Promise.resolve(1), second()]);
  return error;
}

function nodeError() {
  try {
    Buffer.alloc(-1);
  } catch (error) {
    return error;
  }
  throw new Error('Buffer.alloc(-1) did not throw');
}

// Functions each run in one tier when they make their error, forced with
// V8's natives, which code compiled after the flag may call.
function inTurbofan() {
  return new Error('in Turbofan code');
}

function inlinedHere() {
  return new Error('inlined into Turbofan code');
}

function inlining() {
  return inlinedHere();
}

function inMaglev() {
  return new Error('in Maglev code');
}

function inSparkplug() {
  return new Error('in Sparkplug code');
}

/** The error `fn` makes once its code is `tier`'s, which `force` asks for. */
function tiered(natives, fn, force, tier) {
  natives.prepare(fn);
  fn();
  fn();
  force(fn);
  const error = fn();
  const now = natives.tier(fn);
  if (now !== tier) {
    throw new Error(`${fn.name} ran in ${now}, not ${tier}`);
  }
  return error;
}

function tieredErrors() {
  require('node:v8').setFlagsFromString('--allow-natives-syntax');
  /* eslint-disable no-new-func */
  const natives = {
    prepare: new Function('f', '%PrepareFunctionForOptimization(f);'),
    turbofan: new Function('f', '%OptimizeFunctionOnNextCall(f);'),
    maglev: new Function('f', '%OptimizeMaglevOnNextCall(f);'),
    sparkplug: new Function('f', '%CompileBaseline(f);'),
    tier: new Function('f', `return %ActiveTierIsTurbofan(f) ? 'turbofan' :
      %ActiveTierIsMaglev(f) ? 'maglev' : %ActiveTierIsSparkplug(f) ? 'sparkplug' : 'ignition';`),
    hasMaglev: new Function('return %IsMaglevEnabled();'),
  };
  /* eslint-enable no-new-func */
  return {
    turbofan: tiered(natives, inTurbofan, natives.turbofan, 'turbofan'),
    inlined: tiered(natives, inlining, natives.turbofan, 'turbofan'),
    // Node 22 builds V8 without Maglev enabled.
    ...(natives.hasMaglev() && {maglev: tiered(natives, inMaglev, natives.maglev, 'maglev')}),
    sparkplug: tiered(natives, inSparkplug, natives.sparkplug, 'sparkplug'),
  };
}

// Each error but topLevel keeps only the frames its own case makes, so that
// inspecting them all fits in one evaluation's reads.
async function unreadErrors(topLevel) {
  const widget = new Widget();
  widget.describe = describeWidget;
  const limit = Error.stackTraceLimit;
  Error.stackTraceLimit = 4;
  try {
    return await errorsOfEachCase(topLevel, widget);
  } finally {
    Error.stackTraceLimit = limit;
  }
}

async function errorsOfEachCase(topLevel, widget) {
  return {
    topLevel,
    method: widget.describe(),
    constructed: new Gadget().error,
    fieldInitializer: new Fielded().error,
    staticMethod: Shape.create(),
    instanceMethod: new Shape().area(),
    functionMethod: Registry.lookup(),
    getter: withGetter.broken,
    callback: mapOne(),
    evaluated: evaluate(),
    sourceUrl: fromUrl(),
    offsets: fromOffset(),
    afterAwait: await awaitSettle(),
    promiseAll: await gatherAll(),
    node: nodeError(),
    ...tieredErrors(),
  };
}

const topLevel = new SyntaxError('never read');
unreadErrors(topLevel).then(gallery);
