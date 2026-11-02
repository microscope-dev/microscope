// How hard property tests search: their own default of 100 cases per
// property on every save, or PROPTEST_CASES of them for the deeper search
// `just web-props` makes, as `just props` does for the Rust tests.

import fc from "fast-check";

const cases = Number(process.env.PROPTEST_CASES ?? "");
if (Number.isInteger(cases) && cases > 0) {
  fc.configureGlobal({ numRuns: cases });
}
