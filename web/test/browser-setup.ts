// Every browser test starts with an empty workbench, as a fresh page does.

import { afterEach } from "vitest";
import { emptyLayout } from "../src/workbench/layout";
import { workbench } from "../src/workbench/store";

afterEach(() => {
  workbench.setState({ layout: emptyLayout() }, true);
});
