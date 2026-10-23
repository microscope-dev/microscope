// An object whose properties an interceptor gives: process.env, whose
// variables come from the environment when read. Nothing in the object
// itself holds them, so microscope must refuse it instead of showing {}.

'use strict';

const truth = require('./truth.js');

function look() {
  const env = process.env;
  const keep = () => env;
  truth.checkpoint('look', {locals: {}});
  return keep;
}

look();
