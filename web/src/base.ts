// The path the page is served under: `/`, or a reverse proxy's path such
// as `/debug/7/`, which the server names in a meta tag (`--public-url`).

import { assert } from "./assert";

function read(): string {
  const content = document.querySelector<HTMLMetaElement>('meta[name="microscope-base"]')?.content;
  if (!content?.startsWith("/")) {
    return "/";
  }
  return content.endsWith("/") ? content : `${content}/`;
}

export const base = typeof document === "undefined" ? "/" : read();
assert(base.startsWith("/"), `base ${base} does not start with /`);
assert(base.endsWith("/"), `base ${base} does not end with /`);

/** The router's path for a path of this page: the same, less `base`. */
export function withinBase(path: string): string {
  return base !== "/" && path.startsWith(base) ? `/${path.slice(base.length)}` : path;
}
