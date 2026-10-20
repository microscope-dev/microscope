// What every view of a stop's data says when it shows none.

import type { RequestError } from "../protocol";
import type { Focus } from "./Workspace";

/**
 * Why what a stop holds is not shown, or null when it is: `what` names it,
 * such as "Values".
 */
export function hidden(focus: Focus, what = "Values"): string | null {
  const { at, stale } = focus;
  if (!at) {
    return `${what} appear when the program stops.`;
  }
  switch (stale) {
    case "running":
      return `The program is running; ${what.toLowerCase()} appear when it stops.`;
    case "passed":
      return `${what} hidden: they belong to stop #${at.stop}, which has passed.`;
    case "disconnected":
      return "Reconnecting to microscope…";
    default:
      return null;
  }
}

/**
 * The class of a pane's message for a request's error: what the frame does
 * not have, such as an interpreted frame's registers, is said plainly, and
 * only a failure in red.
 */
export function errorClass(error: RequestError): string {
  return error.kind === "unsupported" ? "empty" : "empty error";
}
