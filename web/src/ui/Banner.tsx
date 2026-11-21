import { createMemo, createSignal, Match, Show, Switch } from "solid-js";

import { assert } from "../assert";
import { type At, formatPlace } from "../focus";
import { unreached } from "../follow";
import type { Place } from "../protocol";
import { tab, useTab } from "../tab";
import { useVisible } from "../workbench/visible";
import { threadLabel } from "./names";
import { useGo, useLinkPaths } from "./navigation";
import { fileName } from "./paths";
import { useFocus } from "./Workspace";

export function placeText(place: Place | null | undefined): string {
  if (!place) {
    return "an unknown place";
  }
  if (place.path && place.line) {
    return `${fileName(place.path)}:${place.line}`;
  }
  return place.function ?? place.address;
}

export function Banner() {
  const focus = useFocus();
  const pinned = useTab((current) => current.pinned);
  const [stayed, setStayed] = createSignal<number | null>(null);
  const visible = useVisible();

  const passedAt = createMemo(() => {
    const { at, stale } = focus();
    return visible() && at && stale === "passed" && stayed() !== at.stop ? at : null;
  });

  const pinnedAt = createMemo(() => {
    const { at, stale } = focus();
    return visible() && at && pinned() && stale !== "passed" ? at : null;
  });

  return (
    <Switch>
      <Match when={passedAt()}>{(at) => <Passed at={at()} stay={setStayed} />}</Match>
      <Match when={pinnedAt()}>
        {(at) => (
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a notice, not a form's result
          <div class="stop-banner quiet" role="status" data-testid="pinned">
            <div>
              Pinned to stop #{at().stop}: this tab stays here when the program stops elsewhere.
            </div>
            <button type="button" class="button small" onClick={unpin}>
              Unpin <kbd>p</kbd>
            </button>
          </div>
        )}
      </Match>
    </Switch>
  );
}

const unpin = () => tab.setState({ pinned: false });

function Passed(props: { at: At; stay: (stop: number) => void }) {
  const focus = useFocus();
  const paths = useLinkPaths();
  const go = useGo();
  const state = createMemo(() => focus().state);
  const inferior = createMemo(() => state().inferior);
  const shown = createMemo(() =>
    state().stops.find((candidate) => candidate.stop === props.at.stop),
  );
  const now = createMemo(() => {
    const current = inferior();
    return current.state === "stopped" ? current : null;
  });
  const thread = createMemo(() => {
    const current = now();
    return current ? state().threads.find((candidate) => candidate.id === current.thread) : null;
  });

  const goToNow = () => {
    const current = now();
    if (!current) {
      return;
    }
    tab.setState({ pinned: false });
    go({ stop: current.stop, thread: current.thread, frame: current.frame });
  };

  const goToSession = () => go(null);

  const stayHere = () => {
    const at = props.at;
    const look = focus().look;
    props.stay(at.stop);
    const place = shown()?.place;
    assert(place?.path, `stay on stop ${at.stop} offered with no path`);
    assert(place.line, `stay on stop ${at.stop} offered with no line`);
    if (!look.src) {
      go(
        at,
        {
          ...look,
          src: formatPlace({
            path: paths.link(place.path),
            line: place.line,
            end: place.line,
          }),
        },
        { replace: true },
      );
    }
  };

  const stayPlace = createMemo(() => {
    const place = shown()?.place;
    return place?.path && place.line ? place : null;
  });

  return (
    // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a notice, not a form's result
    <div class="stop-banner" role="status" data-testid="passed">
      <div>
        <b>
          {unreached(state(), props.at.stop)
            ? `This session has not reached stop #${props.at.stop}.`
            : `Stop #${props.at.stop} has passed.`}
        </b>{" "}
        {(() => {
          const current = now();
          if (current) {
            return `The session is at stop #${current.stop}, ${placeText(current.place)} in ${threadLabel(current.thread, thread()?.name)}.`;
          }
          return inferior().state === "running"
            ? "The program is running."
            : "The program is no longer stopped.";
        })()}{" "}
        <span class="muted">Values are hidden: they belong to stop #{props.at.stop}.</span>
      </div>
      <div class="actions">
        <Show
          when={now()}
          fallback={
            <button type="button" class="button small" onClick={goToSession}>
              Go to the session
            </button>
          }
        >
          {(current) => (
            <button type="button" class="button primary small" onClick={goToNow}>
              Go to stop #{current().stop}
            </button>
          )}
        </Show>
        <Show when={stayPlace()}>
          {(place) => (
            <button type="button" class="button small" onClick={stayHere}>
              Stay on {placeText(place())}
            </button>
          )}
        </Show>
      </div>
    </div>
  );
}
