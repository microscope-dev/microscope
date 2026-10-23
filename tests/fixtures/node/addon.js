// JavaScript values held in a Node-API addon's native frame: the addon's
// inspect() keeps them in napi_value locals while it calls back into
// JavaScript, which checkpoints with the frame below it. The first argument
// is the path of the built addon.
//
// The checkpoint names each value the native frame holds by the name of the
// addon's local, with util.inspect's text (truth.js).

'use strict';

const truth = require('./truth.js');
const addon = require(process.argv[2]);

function main() {
  const object = {a: 1};
  const text = 'hello';
  addon.inspect(object, text, () => truth.checkpoint('addon', {locals: {object, text}}));
}

main();
