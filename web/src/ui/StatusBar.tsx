import { useNavigate } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, Match, Show, Switch } from "solid-js";

import { assert } from "../assert";
import { stopPath } from "../focus";
import { toHref } from "../history";
import { stopKind } from "../model";
import type { StopEntry } from "../protocol";
import { useModel } from "../store";
import { useTab } from "../tab";
import { placeText } from "./Banner";

const SHOWN = 8;

const FLASH = 4000;

export function StatusBar() {
  const notices = useModel((model) => model.notices);
  const me = useModel((model) => model.hello?.connection);
  const state = useModel((model) => model.state);
  const flash = useTab((current) => current.flash);
  const now = useNow(() => {
    const current = flash();
    return current ? current.at + FLASH : null;
  });
  const latest = createMemo(() => notices().findLast((notice) => notice.connection !== me()));
  const stops = createMemo(() => {
    const current = state();
    return current?.session ? current.stops.slice(-SHOWN) : [];
  });
  const flashing = createMemo(() => {
    const current = flash();
    return current !== null && now() - current.at < FLASH ? current : null;
  });
  const idle = createMemo(() => stops().length === 0 && !flashing() && !latest());

  return (
    <footer class={["strip status-bar", { idle: idle() }]}>
      <Show when={stops().length > 0}>
        <nav class="stops" aria-label="Stops" data-testid="stops">
          <span class="muted">Stops</span>
          <For each={stops()} keyed={(entry) => entry.stop}>
            {(entry) => <StopButton entry={entry()} class="stop" />}
          </For>
        </nav>
      </Show>
      <span class="end" aria-live="polite">
        <Switch>
          <Match when={flashing()}>
            {(current) => (
              <span class="notice" data-testid="flash">
                {current().text}
              </span>
            )}
          </Match>
          <Match when={latest()}>
            {(current) => (
              <span class="notice" data-testid="notice">
                <b>{current().name}</b> {current().text}{" "}
                <span class="muted">{new Date(current().at).toLocaleTimeString()}</span>
              </span>
            )}
          </Match>
        </Switch>
      </span>
    </footer>
  );
}

export function Stops() {
  const stops = useModel((model) => model.state?.stops);
  const reversed = createMemo(() => stops()?.toReversed() ?? []);
  return (
    <Show
      when={reversed().length > 0}
      fallback={
        <div class="pane-body">
          <div class="empty">Stops appear here as the program stops.</div>
        </div>
      }
    >
      <div class="pane-body">
        <ul class="rows" data-testid="stop-list">
          <For each={reversed()} keyed={(entry) => entry.stop}>
            {(entry) => (
              <li>
                <StopButton entry={entry()} class="row button-row" />
              </li>
            )}
          </For>
        </ul>
      </div>
    </Show>
  );
}

function StopButton(props: { entry: StopEntry; class: string }) {
  const session = useModel((model) => model.state?.session);
  const current = useModel((model) =>
    model.state?.inferior.state === "stopped" ? model.state.inferior.stop : null,
  );
  const at = useTab((state) => state.at);
  const navigate = useNavigate();
  const on = createMemo(() => props.entry.stop === at()?.stop);
  const isCurrent = createMemo(() => props.entry.stop === current());
  const onClick = () => {
    const currentSession = session();
    assert(currentSession, `stop ${props.entry.stop} listed with no session`);
    navigate(
      toHref(
        stopPath(currentSession, {
          stop: props.entry.stop,
          thread: props.entry.thread,
          frame: props.entry.frame,
        }),
      ),
    );
  };
  return (
    <button
      type="button"
      class={[props.class, { on: on(), current: isCurrent() }]}
      aria-current={on() ? "true" : undefined}
      title={describeStop(props.entry)}
      onClick={onClick}
    >
      #{props.entry.stop} {stopKind(props.entry.reason.kind)} · {placeText(props.entry.place)}
      <Show when={isCurrent()}>{" ◂ now"}</Show>
    </button>
  );
}

function describeStop(entry: StopEntry): string {
  const who = entry.by ? `${entry.by} ${entry.action ?? "ran the program"} · ` : "";
  return `#${entry.stop} · ${who}${entry.reason.description} · ${placeText(entry.place)}`;
}

function useNow(deadline: () => number | null): () => number {
  const [now, setNow] = createSignal(Date.now());
  createEffect(
    () => deadline(),
    (until) => {
      if (until === null) {
        return;
      }
      const timer = setTimeout(() => setNow(Date.now()), Math.max(0, until - Date.now()));
      return () => clearTimeout(timer);
    },
  );
  return now;
}
