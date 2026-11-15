
'use strict';
function relay(callback, value) {
  const result = callback(value);
  return result;
}
module.exports = { relay };
