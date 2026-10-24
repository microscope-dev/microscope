// A library a step passes through when its directory is skipped.
'use strict';
function relay(callback, value) {
  const result = callback(value);
  return result;
}
module.exports = { relay };
