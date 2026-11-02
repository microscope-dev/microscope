// The journeys of test/driver/journeys.ts against a real `microscope web`,
// in Chromium and Firefox. test/journeys.test.tsx runs the same journeys
// against a scripted debugger, in milliseconds.

import { breakStepAndRead, switchPanes } from "../test/driver/journeys";
import { RealUser } from "../test/driver/real";
import { fixture, join, test } from "./server";

test.use({ program: [fixture("kvstore")] });

test("breaks, steps over a line, and reads the local it set", async ({ page, microscope }) => {
  await join(page, microscope.link);
  await breakStepAndRead(new RealUser(page));
});

test("moves among the debug panes by tab, key, and menu", async ({ page, microscope }) => {
  await join(page, microscope.link);
  await switchPanes(new RealUser(page));
});
