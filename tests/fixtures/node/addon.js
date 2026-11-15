







'use strict';

const truth = require('./truth.js');
const addon = require(process.argv[2]);

function main() {
  const object = {a: 1};
  const text = 'hello';
  addon.inspect(object, text, () => truth.checkpoint('addon', {locals: {object, text}}));
}

main();
