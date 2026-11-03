// Services' cookies as a page served over plain HTTP keeps them, for the
// development server's proxy (vite.config.ts) and the browser tests'
// (e2e/services.ts). Services' cookies are `__Host-` cookies, which a
// browser keeps only over HTTPS, so the browser holds them as `dev-`
// cookies, without Secure. Single sign-on's is SameSite=None, which needs
// Secure, so it becomes Lax, which still comes back from an identity
// provider on the page's own site, such as services' mock.

/** A Cookie header from the browser, as services reads it. Every port on
 * localhost shares one cookie jar, so the browser may also send an older
 * cookie of the same name with a narrower path, ahead of the newest; a
 * `__Host-` cookie it sends is from somewhere other than this page. Only
 * the last `dev-` cookie of each name, the newest at `/`, goes on. */
export function toServices(header: string): string {
  const kept = new Map<string, string>();
  for (const pair of header.split(/;\s*/)) {
    const name = pair.slice(0, pair.indexOf("="));
    if (name.startsWith("dev-")) {
      kept.set(name, `__Host-${pair.slice("dev-".length)}`);
    } else if (!name.startsWith("__Host-")) {
      kept.set(`${name}\0${kept.size}`, pair);
    }
  }
  return [...kept.values()].join("; ");
}

/** A Set-Cookie header from services, as the browser should keep it. */
export function fromServices(cookie: string): string {
  return cookie
    .replace(/^__Host-/, "dev-")
    .replace(/;\s*Secure/i, "")
    .replace(/;\s*SameSite=None/i, "; SameSite=Lax");
}
