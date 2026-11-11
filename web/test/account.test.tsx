// Who the page says is signed in: an answer services gives to a question
// asked before the page itself signed in or out never undoes it.

import { afterEach, expect, it, vi } from "vitest";

import { account, checkAccount, signOut, totpConfirmed, type User } from "../src/account";

const ME: User = { id: 1, username: "ana", email: "ana@example.com", totp_enabled: true };

afterEach(() => {
  vi.unstubAllGlobals();
  account.setState({ now: { kind: "checking" }, notice: null });
});

/** Services, holding each answer to who is signed in until the test gives
 * it, and signing out at once. */
function services() {
  const held: ((user: User | null) => void)[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn((input: RequestInfo | URL) =>
      String(input) === "/api/users/me"
        ? new Promise<Response>((resolve) => {
            held.push((user) =>
              resolve(
                user
                  ? new Response(JSON.stringify(user), { status: 200 })
                  : new Response(null, { status: 401 }),
              ),
            );
          })
        : Promise.resolve(new Response(null, { status: 204 })),
    ),
  );
  return held;
}

it.each([
  ["a check", checkAccount],
  ["a refresh after a change to the account", totpConfirmed],
])("keeps a sign-out that lands while %s is asked", async (_, question) => {
  const held = services();
  account.setState({ now: { kind: "in", user: { ...ME, totp_enabled: false } } });
  const asked = question();
  await vi.waitFor(() => expect(held).toHaveLength(1));

  await signOut();
  expect(account.getState().now).toEqual({ kind: "out" });

  // Services answers as it was before the sign-out.
  held[0]?.(ME);
  await asked;
  expect(account.getState().now).toEqual({ kind: "out" });
});
