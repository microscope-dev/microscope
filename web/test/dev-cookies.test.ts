import { describe, expect, it } from "vitest";

import { fromServices, toServices } from "../dev-cookies";

describe("services' cookies over plain HTTP", () => {
  it("forwards only the newest of each, under services' name", () => {
    // The browser sends the narrower path first, then oldest to newest.
    const header =
      "dev-session=stale; theme=dark; dev-session=fresh; __Host-session=elsewhere; dev-sso=s";
    expect(toServices(header)).toBe("__Host-session=fresh; theme=dark; __Host-sso=s");
  });

  it("keeps a cookie the browser would refuse over HTTP", () => {
    expect(fromServices("__Host-sso=t; Max-Age=600; Path=/; HttpOnly; Secure; SameSite=None")).toBe(
      "dev-sso=t; Max-Age=600; Path=/; HttpOnly; SameSite=Lax",
    );
  });
});
