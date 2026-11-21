import { useLocation, useNavigate } from "@solidjs/router";
import { createEffect, createMemo, createSignal, Show } from "solid-js";

import { tradeOnce } from "../connection";
import { isPagePath, parseSearch } from "../focus";
import { toHref } from "../history";
import { useConnection } from "../store";

let token: string | null = null;
const trade = tradeOnce();

function join(): Promise<boolean> {
  if (token === null) {
    token = window.location.hash.slice(1);
    window.history.replaceState(null, "", window.location.pathname + window.location.search);
  }
  return trade(token);
}

const CARD = { width: "min(560px, 100%)" };
const NO_MARGIN = { margin: "0" };

export function Join() {
  const connection = useConnection();
  const navigate = useNavigate();
  const location = useLocation();
  const to = createMemo(() => {
    const target = parseSearch(location.search).to;
    return typeof target === "string" ? target : undefined;
  });
  const [failed, setFailed] = createSignal<"refused" | "unreachable" | null>(null);
  const [tries, setTries] = createSignal(0);

  createEffect(
    () => [to(), tries()] as const,
    ([target]) => {
      const attempt = async () => {
        if (!(await join())) {
          setFailed("refused");
          return;
        }
        connection.start();
        navigate(isPagePath(target) ? toHref(target) : toHref("/"), { replace: true });
      };
      attempt().catch(() => setFailed("unreachable"));
    },
  );
  const retry = () => {
    setFailed(null);
    setTries((count) => count + 1);
  };

  return (
    <Show when={failed()} fallback={<div class="page muted">Joining…</div>}>
      {(reason) => (
        <div class="page">
          <div class="card" style={CARD}>
            <div class="card-head">
              <h1>{reason() === "refused" ? "Open a join link" : "Could not join"}</h1>
            </div>
            <div class="card-body">
              <p style={NO_MARGIN}>
                {reason() === "refused"
                  ? "This link is not for this microscope, or it has expired. microscope web prints one when it starts, and Share makes more."
                  : "Could not reach microscope to use this link. Check that it is running, then try again."}
              </p>
              <Show when={reason() === "unreachable"}>
                <div class="actions">
                  <button type="button" class="button primary" onClick={retry}>
                    Try again
                  </button>
                </div>
              </Show>
            </div>
          </div>
        </div>
      )}
    </Show>
  );
}
