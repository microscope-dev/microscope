// How hard property tests search: their own default of 100 cases per
// property on every save, or PROPTEST_CASES of them for the deeper search
// `just web-props` makes, as `just props` does for the Rust tests.

import * as fc from "fast-check";
import { inject } from "vitest";

declare module "vitest" {
  export interface ProvidedContext {
    /** PROPTEST_CASES, or "" when it is not set. */
    proptestCases: string;
  }
}

const cases = Number(inject("proptestCases"));
if (Number.isInteger(cases) && cases > 0) {
  fc.configureGlobal({ numRuns: cases });
}
