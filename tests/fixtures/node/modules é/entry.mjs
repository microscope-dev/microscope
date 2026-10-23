// An ES module, in a directory whose name a file: URL percent-encodes, that
// calls into a TypeScript module Node strips the types of. V8 names both
// scripts by file: URLs.
//
// main:  <module top level> > settle (entry.mjs) > total (helper.mts) > checkpoint("modules")

import truth from '../truth.js';
import {total} from './helper.mts';

function settle(entries) {
  return total(entries, () => truth.checkpoint('modules', {locals: {entries}}));
}

settle([4, 5, 6]);
truth.release();
