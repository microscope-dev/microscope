export function toServices(header: string): string {
  const kept = new Map<string, string>();
  for (const pair of header.split(/;\s*/u)) {
    const name = pair.slice(0, pair.indexOf("="));
    if (name.startsWith("dev-")) {
      kept.set(name, `__Host-${pair.slice("dev-".length)}`);
    } else if (!name.startsWith("__Host-")) {
      kept.set(`${name}\0${kept.size}`, pair);
    }
  }
  return [...kept.values()].join("; ");
}

export function fromServices(cookie: string): string {
  return cookie
    .replace(/^__Host-/u, "dev-")
    .replace(/;\s*Secure/iu, "")
    .replace(/;\s*SameSite=None/iu, "; SameSite=Lax");
}
