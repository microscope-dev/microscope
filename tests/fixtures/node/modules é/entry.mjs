// An ES module, in a directory whose name a file: URL percent-encodes, that
// calls into a TypeScript module Node strips the types of. V8 names both
// scripts by file: URLs.
//
// main:  <module top level> > settle (entry.mjs) > total (helper.mts) > checkpoint("modules")

import truth from '../truth.js';
import {total} from './helper.mts';

// The callback captures a module binding and one of settle's locals, which
// a debugger shows in separate scopes.
const label = 'modules';

function settle(entries) {
  return total(entries, () => truth.checkpoint(label, {locals: {entries}}));
}

settle([4, 5, 6]);
truth.release();
