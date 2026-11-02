// Journeys through the page, written once against `User`. Each names the
// program it expects, which each backend provides: the real fixture, or a
// script of it (scripted.ts).

import type { User } from "./user";

/** In kvstore, breaks where handle_request starts its answer, steps over
 * that line, and reads the local it set. */
export async function breakStepAndRead(user: User): Promise<void> {
  await user.addBreakpoint("kvstore.c:92");
  await user.press("F5");
  await user.expectCurrentLine("int status = 0;");
  await user.press("F10");
  await user.expectCurrentLine("switch (req->op) {");
  await user.expectValue("Variables", "status", "0");
}
