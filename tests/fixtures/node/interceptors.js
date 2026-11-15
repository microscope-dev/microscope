



'use strict';

const truth = require('./truth.js');

function look() {
  const env = process.env;
  const keep = () => env;
  truth.checkpoint('look', {locals: {}});
  return keep;
}

look();
