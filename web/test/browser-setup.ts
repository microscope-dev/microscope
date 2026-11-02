// Every browser test starts with an empty workbench and nothing kept in the
// browser, as a fresh page does.

import { afterEach } from "vitest";
import { resetWorkbench } from "../src/workbench/store";

afterEach(() => {
  resetWorkbench();
  localStorage.clear();
});
