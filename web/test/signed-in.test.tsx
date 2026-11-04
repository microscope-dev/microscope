// Coming back from single sign-on to `/signed-in#…`: what the fragment
// carried reaches the Account pane, even when it is followed twice, as
// React's development build runs every effect twice.

import { afterEach, expect, it, vi } from "vitest";

import { account, followSignIn } from "../src/account";

afterEach(() => {
  vi.unstubAllGlobals();
  account.setState({ now: { kind: "checking" }, notice: null, challenge: null });
});

it("keeps the error and the challenge when followed a second time", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response(null, { status: 401 })),
  );
  window.history.replaceState(null, "", "/signed-in#error=An%20account%20already%20uses%20it.");
  followSignIn();
  followSignIn();
  expect(window.location.hash).toBe("");
  await expect.poll(() => account.getState().now.kind).toBe("out");
  expect(account.getState().notice).toEqual({ ok: false, text: "An account already uses it." });

  window.history.replaceState(null, "", "/signed-in#challenge=abc&method=totp");
  followSignIn();
  followSignIn();
  expect(account.getState().challenge).toBe("abc");
});
