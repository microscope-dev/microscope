import { base, bundled } from "./base";
import { read, write } from "./storage";

export interface Remote {
  server: string;
  token: string;
}

export const standalone = !bundled;

const KEY = "microscope-remote";

const isRemote = (value: unknown): value is Remote =>
  typeof value === "object" &&
  value !== null &&
  typeof (value as Remote).server === "string" &&
  typeof (value as Remote).token === "string";

const isChoice = (value: unknown): value is Remote | null => value === null || isRemote(value);

export function remote(): Remote | null {
  if (!standalone) {
    return null;
  }
  const tab = read<Remote | null | undefined>(KEY, undefined, isChoice, "session");
  if (tab !== undefined) {
    return tab;
  }

  const latest = read<Remote | null>(KEY, null, isRemote);
  if (latest) {
    write(KEY, latest, "session");
  }
  return latest;
}

export function setRemote(chosen: Remote | null): void {
  write(KEY, chosen, "session");
  write(KEY, chosen);
}

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

export function serverUrl(path: string): string {
  return `${remote()?.server ?? base}${path}`;
}

export function presented(): string {
  return remote()?.token ?? "";
}
