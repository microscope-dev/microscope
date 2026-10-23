// Values of Node's library types and of the program's own classes, which
// views present by the classes their prototypes name, at the checkpoint
// "views" in the frame of views(), each with its canonical text
// (truth.js).

'use strict';

const EventEmitter = require('node:events');
const truth = require('./truth.js');

class Point {
  constructor(x, y) {
    this.x = x;
    this.y = y;
  }
}

class Point3 extends Point {
  constructor(x, y, z) {
    super(x, y);
    this.z = z;
  }
}

class Emitter extends EventEmitter {}

function views() {
  const emitter = new EventEmitter();
  emitter.on('data', function first() {});
  emitter.on('data', function second() {});
  emitter.once('end', function ended() {});
  const derived = new Emitter();
  derived.on('tick', function tick() {});
  const values = {
    buffer: Buffer.from([1, 2, 3, 250]),
    uint8: new Uint8Array([4, 5, 6]),
    floats: new Float64Array([1.5, -2, 0.25]),
    emitter,
    derived,
    bare: Object.create(EventEmitter.prototype),
    url: new URL('https://example.com/a?b=1#c'),
    point: new Point(1, 2),
    point3: new Point3(1, 2, 3),
  };
  const locals = Object.fromEntries(
    Object.entries(values).map(([name, value]) => [`values.${name}`, value]));
  locals['values.emitter._events'] = emitter._events;
  locals['values.derived._events'] = derived._events;
  // A closure that captures values keeps it in the frame's context, where
  // its scope names it at any stop.
  const captured = () => values;
  truth.checkpoint('views', {locals});
  return captured;
}

views();
