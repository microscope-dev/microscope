'use strict';
// microscope's launch preload. In a process microscope's tracer traces, it
// opens Node's inspector and waits for microscope to connect; elsewhere it
// does nothing.
(() => {
  const env = process.env;
  const tracer = env.MICROSCOPE_TRACER;
  const ownership = env.MICROSCOPE_NODE_OWNERSHIP;
  if (!require('worker_threads').isMainThread || !tracer) return;
  let status;
  try {
    status = require('fs').readFileSync('/proc/self/status', 'latin1');
  } catch {
    status = '';
  }
  const traced = /^TracerPid:\s*(\d+)$/m.exec(status)?.[1] === tracer;
  if (!traced || env.MICROSCOPE_NODE_FOLLOW !== '1') {
    const original = env.MICROSCOPE_NODE_OPTIONS ?? '0';
    if (original.startsWith('1')) env.NODE_OPTIONS = original.slice(1);
    else delete env.NODE_OPTIONS;
    delete env.MICROSCOPE_NODE_OPTIONS;
    delete env.MICROSCOPE_TRACER;
    delete env.MICROSCOPE_NODE_FOLLOW;
    delete env.MICROSCOPE_NODE_OWNERSHIP;
  }
  if (!traced) return;
  const inspector = require('inspector');
  try {
    const borrowed = Boolean(inspector.url());
    if (!borrowed) inspector.open(0, '127.0.0.1', false);
    const target = inspector.url()?.split('/').pop();
    inspector.waitForDebugger();
    if (ownership && target) {
      inspector.console.debug('microscope ownership', ownership, target, borrowed ? 'borrowed' : 'created');
    }
  } catch {
    // node --test's runner has no inspector (F30).
  }
})();
