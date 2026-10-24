// A library with a debugger statement, which stops unless its directory
// is skipped.
'use strict';
function halt() {
  debugger; // HALT
  return 1;
}
module.exports = { halt };
