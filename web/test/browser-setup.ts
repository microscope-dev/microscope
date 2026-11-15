import { afterEach } from "vitest";

import { resetWorkbench } from "../src/workbench/store";

afterEach(() => {
  resetWorkbench();
  localStorage.clear();
});
