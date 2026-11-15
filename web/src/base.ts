import { assert } from "./assert";

const meta =
  typeof document === "undefined"
    ? null
    : document.querySelector<HTMLMetaElement>('meta[name="microscope-base"]');

export const bundled = meta !== null;

function read(): string {
  const content = meta?.content;
  if (!content?.startsWith("/")) {
    return "/";
  }
  return content.endsWith("/") ? content : `${content}/`;
}

export const base = read();
assert(base.startsWith("/"), `base ${base} does not start with /`);
assert(base.endsWith("/"), `base ${base} does not end with /`);

export function withinBase(path: string): string {
  return base !== "/" && path.startsWith(base) ? `/${path.slice(base.length)}` : path;
}
