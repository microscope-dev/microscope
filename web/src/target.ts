// Which microscope the page talks to. Served by `microscope web`, it is the
// server that served it, which keeps the token in a cookie. Served on its
// own, by `just web` or any static server, it is the one whose join link the
// user gave, and the page presents the link's token with each request.
//
// Each tab keeps its own, so connecting one tab to another microscope leaves
// the others where they are, even when they reconnect or reload. A new tab
// starts with the one chosen last in any tab.

import { base, bundled } from "./base";
import { read, write } from "./storage";

export interface Remote {
  /** The server's URL, ending with a slash, such as `http://127.0.0.1:7341/`. */
  server: string;
  token: string;
}

/** Whether the page was served on its own and must be told which microscope to use. */
export const standalone = !bundled;

const KEY = "microscope-remote";

const isRemote = (value: unknown): value is Remote =>
  typeof value === "object" &&
  value !== null &&
  typeof (value as Remote).server === "string" &&
  typeof (value as Remote).token === "string";

/** A tab's choice, which may be to have none. */
const isChoice = (value: unknown): value is Remote | null => value === null || isRemote(value);

/** The microscope a page served on its own talks to, if it was given one. */
export function remote(): Remote | null {
  if (!standalone) {
    return null;
  }
  const tab = read<Remote | null | undefined>(KEY, undefined, isChoice, "session");
  if (tab !== undefined) {
    return tab;
  }
  // A tab takes the latest choice the first time it looks, and keeps it. A
  // tab with none yet takes one made later, when it is reloaded.
  const latest = read<Remote | null>(KEY, null, isRemote);
  if (latest) {
    write(KEY, latest, "session");
  }
  return latest;
}

/** Chooses this tab's microscope, and the one new tabs start with. */
export function setRemote(chosen: Remote | null): void {
  write(KEY, chosen, "session");
  write(KEY, chosen);
}

/** Reads a link `microscope web` printed or Share made, such as
 * `http://127.0.0.1:7341/join?to=%2Fs%2Fab#c-1234`. */
export function parseJoinLink(text: string): (Remote & { to: string }) | null {
  let url: URL;
  try {
    url = new URL(text.trim());
  } catch {
    return null;
  }
  const token = url.hash.slice(1);
  if (!["http:", "https:"].includes(url.protocol) || !url.pathname.endsWith("/join") || !token) {
    return null;
  }
  return {
    server: `${url.origin}${url.pathname.slice(0, -"join".length)}`,
    token,
    to: url.searchParams.get("to") ?? "/",
  };
}

/** The URL of one of the server's paths, such as `api/check`. */
export function serverUrl(path: string): string {
  return `${remote()?.server ?? base}${path}`;
}

/** The body of a POST to the server: the token, or nothing for the cookie to stand in. */
export function presented(): string {
  return remote()?.token ?? "";
}
