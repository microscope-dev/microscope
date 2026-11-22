import { useLocation } from "@solidjs/router";
import { createEffect, createSignal, Match, Show, Switch } from "solid-js";

import { checkAccount, failure, signIn, useAccount } from "../account";
import { parseSearch } from "../focus";

const CARD = { width: "min(460px, 100%)" };

export function CliLogin() {
  const location = useLocation();
  const account = useAccount();

  const [code, setCode] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal<string | null>(null);
  const [approved, setApproved] = createSignal(false);

  const [identifier, setIdentifier] = createSignal("");
  const [password, setPassword] = createSignal("");

  createEffect(
    () => location.search,
    (search) => {
      const raw = parseSearch(search).code;
      if (typeof raw === "string" && raw.trim()) {
        setCode((prev) => prev || raw.trim().toUpperCase());
      }
    },
  );

  createEffect(
    () => account().kind,
    (kind) => {
      if (kind === "checking") {
        void checkAccount();
      }
    },
  );

  const onSignIn = async (event: SubmitEvent) => {
    event.preventDefault();
    setBusy(true);
    setErr(null);
    try {
      const res = await signIn(identifier().trim(), password());
      if ("challenge" in res) {
        setErr("Two-factor verification required — sign in on the main page first, then reload.");
      }
    } catch (error: unknown) {
      setErr(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  const onApprove = async (event: SubmitEvent) => {
    event.preventDefault();
    const cleaned = code().trim().toUpperCase();
    if (!cleaned) {
      setErr("Enter the confirmation code printed by `microscope login`.");
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      const response = await fetch("/api/cli/login/approve", {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ user_code: cleaned }),
      });
      if (!response.ok) {
        throw await failure(response);
      }
      setApproved(true);
    } catch (error: unknown) {
      setErr(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="page">
      <div class="card" style={CARD}>
        <div class="card-head">
          <h1>Approve CLI sign-in</h1>
        </div>
        <div class="card-body">
          <Show when={err()}>
            {(msg) => (
              <p class="error" role="alert">
                {msg()}
              </p>
            )}
          </Show>

          <Switch fallback={<p class="muted">Checking session…</p>}>
            <Match when={approved()}>
              <div data-testid="cli-login-approved">
                <p>
                  <strong>CLI sign-in approved.</strong> You can return to your terminal and close
                  this tab.
                </p>
              </div>
            </Match>

            <Match when={account().kind === "in"}>
              <form class="data-panel" onSubmit={(e) => void onApprove(e)}>
                <p style={{ margin: "0" }}>
                  Signed in as{" "}
                  <strong>
                    {account().kind === "in"
                      ? (account() as { kind: "in"; user: { username: string } }).user.username
                      : ""}
                  </strong>
                  . Confirm the code shown in your terminal to issue a personal CLI token for symbol
                  uploads and debuginfod access.
                </p>
                <label class="data-inline-field">
                  <span>Confirmation code</span>
                  <input
                    type="text"
                    class="input mono"
                    aria-label="Confirmation code"
                    placeholder="ABCD-1234"
                    value={code()}
                    onInput={(e) => setCode(e.currentTarget.value.toUpperCase())}
                    required
                  />
                </label>
                <div class="actions">
                  <button
                    type="submit"
                    class="button primary"
                    disabled={busy()}
                    data-testid="cli-login-approve-btn"
                  >
                    {busy() ? "Approving…" : "Approve sign-in"}
                  </button>
                </div>
              </form>
            </Match>

            <Match when={account().kind === "out" || account().kind === "unavailable"}>
              <form class="data-panel" onSubmit={(e) => void onSignIn(e)}>
                <p style={{ margin: "0" }}>
                  Sign in to Microscope first to approve CLI code{" "}
                  <strong class="mono">{code() || "(enter after sign-in)"}</strong>.
                </p>
                <input
                  type="text"
                  class="input"
                  placeholder="Username or email"
                  aria-label="Username or email"
                  value={identifier()}
                  onInput={(e) => setIdentifier(e.currentTarget.value)}
                  required
                />
                <input
                  type="password"
                  class="input"
                  placeholder="Password"
                  aria-label="Password"
                  value={password()}
                  onInput={(e) => setPassword(e.currentTarget.value)}
                  required
                />
                <div class="actions">
                  <button type="submit" class="button primary" disabled={busy()}>
                    {busy() ? "Signing in…" : "Sign in"}
                  </button>
                </div>
              </form>
            </Match>
          </Switch>
        </div>
      </div>
    </div>
  );
}
