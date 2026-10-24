'use strict';
// microscope's launch preload. In a process microscope's tracer traces, it
// opens Node's inspector and waits for microscope to connect; elsewhere it
// does nothing.
(() => {
  const env = process.env;
  const tracer = env.MICROSCOPE_TRACER;
  if (env.MICROSCOPE_NODE_FOLLOW !== '1') {
    const original = env.MICROSCOPE_NODE_OPTIONS ?? '0';
    if (original.startsWith('1')) env.NODE_OPTIONS = original.slice(1);
    else delete env.NODE_OPTIONS;
    delete env.MICROSCOPE_NODE_OPTIONS;
    delete env.MICROSCOPE_TRACER;
  }
  if (!require('worker_threads').isMainThread || !tracer) return;
  let status;
  try {
    status = require('fs').readFileSync('/proc/self/status', 'latin1');
  } catch {
    return;
  }
  if (/^TracerPid:\s*(\d+)$/m.exec(status)?.[1] !== tracer) return;
  const inspector = require('inspector');
  try {
    inspector.open(0, '127.0.0.1', false);
    inspector.waitForDebugger();
  } catch {
    // node --test's runner has no inspector (F30).
  }
})();
