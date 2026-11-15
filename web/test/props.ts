import * as fc from "fast-check";
import { inject } from "vitest";

declare module "vitest" {
  export interface ProvidedContext {
    proptestCases: string;
  }
}

const cases = Number(inject("proptestCases"));
if (Number.isInteger(cases) && cases > 0) {
  fc.configureGlobal({ numRuns: cases });
}
