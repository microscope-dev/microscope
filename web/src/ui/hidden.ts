import { unreached } from "../follow";
import type { RequestError } from "../protocol";
import type { Focus } from "./Workspace";

export function hidden(focus: Focus, what = "Values"): string | null {
  const { at, stale, state } = focus;
  if (!at) {
    return `${what} appear when the program stops.`;
  }
  switch (stale) {
    case "running":
      return `The program is running; ${what.toLowerCase()} appear when it stops.`;
    case "passed":
      return unreached(state, at.stop)
        ? `${what} hidden: the session has not reached stop #${at.stop}.`
        : `${what} hidden: they belong to stop #${at.stop}, which has passed.`;
    case "disconnected":
      return "Reconnecting to microscope…";
    default:
      return null;
  }
}

export function errorClass(error: RequestError): string {
  return error.kind === "unsupported" ? "empty" : "empty error";
}
