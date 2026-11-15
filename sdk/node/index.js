
























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




const checked = process.env.MICROSCOPE_SDK_CHECKS === '1';

function assert(condition, message) {
  if (checked && !condition) {
    throw new Error(`microscope: assertion failed: ${message}`);
  }
}


const problem = native.problem ?? null;
const usable = problem === null;


const sites = new Map();

function site(kind, name, [file, line]) {
  assert(kind >= ZONE && kind <= TRIGGER, 'a site kind out of range');
  if (!usable) {
    return 0n;
  }
  const key = `${kind}\0${name}\0${file}\0${line}`;
  let found = sites.get(key);
  if (found === undefined) {
    found = native.define(kind, String(name), String(file), line);
    assert(typeof found === 'bigint', `the addon defined a site as a ${typeof found}`);
    sites.set(key, found);
  }
  return found;
}


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

  const call = calls[1];
  return call ? [call.getFileName() ?? '', call.getLineNumber() ?? 0] : ['', 0];
}



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


function zone(name, fn) {
  refuse(fn);
  return zoned(site(ZONE, name, caller()), fn, undefined, []);
}


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


function plot(name, value) {
  const at = site(PLOT, name, caller());
  if (at !== 0n) {
    native.plot(at, Number(value));
  }
}



function message(name, text) {
  const at = site(MESSAGE, name, caller());
  if (at !== 0n && native.on(at)) {
    native.message(at, String(typeof text === 'function' ? text() : text));
  }
}


function frame(name = 'frame') {
  const at = site(FRAME, name, caller());
  if (at !== 0n) {
    native.event(at);
  }
}



function trigger(why) {
  const at = site(TRIGGER, why, caller());
  if (at !== 0n) {
    native.event(at);
  }
}


function listening() {
  return usable && native.listening();
}



function found() {
  return usable && native.found();
}

module.exports = {zone, wrap, label, plot, message, frame, trigger, listening, found, problem};
