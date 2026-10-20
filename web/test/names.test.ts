// Every pane names a thread the same way.

import { expect, it } from "vitest";
import { threadLabel } from "../src/ui/names";

it("names a thread by its number, and its name when it has one", () => {
  expect(threadLabel(7, "worker")).toBe("thread 7 (worker)");
  expect(threadLabel(7, null)).toBe("thread 7");
  expect(threadLabel(7)).toBe("thread 7");
  expect(threadLabel(7, "")).toBe("thread 7");
});
