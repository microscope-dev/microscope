// Instrumentation that microscope records, for Node programs: zones,
// plots, messages, labels, frames, and triggers.
//
//     const microscope = require('microscope');
//
//     const handle = microscope.wrap('handle', (request) =>
//       microscope.label('route', request.route, () => {
//         const rows = microscope.zone('query', () => query(request));
//         microscope.plot('rows', rows.length);
//         if (rows.length === 0) {
//           microscope.message('empty', () => `no rows for ${request.id}`);
//         }
//         return render(rows);
//       }));
//
// Sites are defined on first use by name and source location, and stay off
// until a recording enables them by name. Enabled sites write to a thread's
// in-process ring, which microscope reads externally; nothing is sent over
// the network. Sites disable themselves when microscope stops reading.
//
// The microscope.node addon loads the probes SDK's shared library from
// beside itself or MICROSCOPE_PROBES_LIBRARY. The addon is found beside this
// file or at MICROSCOPE_NODE_ADDON. If either is missing, calls do nothing
// and `problem` says why.

'use strict';

const path = require('node:path');

const ZONE = 1;
const FRAME = 2;
const PLOT = 3;
const MESSAGE = 4;
const LABEL = 5;
const TRIGGER = 6;

function load() {
  const addon = process.env.MICROSCOPE_NODE_ADDON || path.join(__dirname, 'microscope.node');
  try {
    return require(addon);
  } catch (error) {
    return {problem: `the addon could not be loaded: ${error.message}`};
  }
}

const native = load();

// Why the package does nothing, or null.
const problem = native.problem ?? null;
const usable = problem === null;

// Each site, by its kind, name, file, and line; 0n is no site.
const sites = new Map();

function site(kind, name, [file, line]) {
  if (!usable) {
    return 0n;
  }
  const key = `${kind}\0${name}\0${file}\0${line}`;
  let found = sites.get(key);
  if (found === undefined) {
    found = native.define(kind, String(name), String(file), line);
    sites.set(key, found);
  }
  return found;
}

// The file and line of the code that called the package's function.
function caller() {
  const prepare = Error.prepareStackTrace;
  const limit = Error.stackTraceLimit;
  let calls;
  try {
    Error.prepareStackTrace = (_, given) => given;
    Error.stackTraceLimit = 3;
    const holder = {};
    Error.captureStackTrace(holder, caller);
    calls = holder.stack;
  } finally {
    Error.prepareStackTrace = prepare;
    Error.stackTraceLimit = limit;
  }
  // calls[0] is the package's function, calls[1] its caller.
  const call = calls[1];
  return call ? [call.getFileName() ?? '', call.getLineNumber() ?? 0] : ['', 0];
}

// Async functions and generators return before finishing; a thread-local
// zone cannot span their suspension.
function suspends(fn) {
  const kind = Object.prototype.toString.call(fn);
  return kind === '[object AsyncFunction]' || kind === '[object GeneratorFunction]' ||
    kind === '[object AsyncGeneratorFunction]';
}

function refuse(fn) {
  if (typeof fn !== 'function') {
    throw new TypeError('a zone spans a function');
  }
  if (suspends(fn)) {
    throw new TypeError('a zone is a span of one thread, and cannot span what an async ' +
      'function or generator awaits or yields');
  }
}

function zoned(at, fn, self, args) {
  const began = at !== 0n && native.zoneBegin(at);
  try {
    return fn.apply(self, args);
  } finally {
    if (began) {
      native.zoneEnd(at);
    }
  }
}

// Records `fn` as a thread-local span named `name` and returns its result.
function zone(name, fn) {
  refuse(fn);
  return zoned(site(ZONE, name, caller()), fn, undefined, []);
}

// Wraps `fn` so each call records a thread-local span named `name`.
function wrap(name, fn) {
  refuse(fn);
  const at = site(ZONE, name, caller());
  if (at === 0n) {
    return fn;
  }
  return function zonedCall(...args) {
    return zoned(at, fn, this, args);
  };
}

// Runs `fn` with samples and zones labelled `name=value`, truncating the
// value to 256 bytes. Returns `fn`'s result.
function label(name, value, fn) {
  refuse(fn);
  const at = site(LABEL, name, caller());
  const began = at !== 0n && native.on(at) && native.labelBegin(at, String(value));
  try {
    return fn();
  } finally {
    if (began) {
      native.labelEnd(at);
    }
  }
}

// A value of the series `name`.
function plot(name, value) {
  const at = site(PLOT, name, caller());
  if (at !== 0n) {
    native.plot(at, Number(value));
  }
}

// A message on the thread's timeline at the site `name`, cut to 256 bytes.
// Pass a function to form its text only while the site is on.
function message(name, text) {
  const at = site(MESSAGE, name, caller());
  if (at !== 0n && native.on(at)) {
    native.message(at, String(typeof text === 'function' ? text() : text));
  }
}

// The end of a frame, for a program that works in frames.
function frame(name = 'frame') {
  const at = site(FRAME, name, caller());
  if (at !== 0n) {
    native.event(at);
  }
}

// Asks microscope to save the recording's window around this moment, when
// it keeps one.
function trigger(why) {
  const at = site(TRIGGER, why, caller());
  if (at !== 0n) {
    native.event(at);
  }
}

// Whether microscope is recording the program's instrumentation.
function listening() {
  return usable && native.listening();
}

module.exports = {zone, wrap, label, plot, message, frame, trigger, listening, problem};
