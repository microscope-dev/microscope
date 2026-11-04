import { useCallback, useState } from "react";

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

/** Where a stop was, as briefly as it can be said. */
export function placeText(place: Place | null | undefined): string {
  if (!place) {
    return "an unknown place";
  }
  if (place.path && place.line) {
    return `${fileName(place.path)}:${place.line}`;
  }
  return place.function ?? place.address;
}

/** Says when the tab shows a stop the program has left, and offers the way back. */
export function Banner() {
  const { at, stale } = useFocus();
  const pinned = useTab((current) => current.pinned);
  const [stayed, setStayed] = useState<number | null>(null);
  // Each document pane has a banner; only the one on screen shows it.
  const visible = useVisible();
  if (!at || !visible) {
    return null;
  }
  if (stale === "passed" && stayed !== at.stop) {
    return <Passed at={at} stay={setStayed} />;
  }
  if (pinned && stale !== "passed") {
    return (
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a notice, not a form's result
      <div className="stop-banner quiet" role="status" data-testid="pinned">
        <div>Pinned to stop #{at.stop}: this tab stays here when the program stops elsewhere.</div>
        <button type="button" className="button small" onClick={unpin}>
          Unpin <kbd>p</kbd>
        </button>
      </div>
    );
  }
  return null;
}

const unpin = () => tab.setState({ pinned: false });

/** The stop shown has passed: where the session is now, and the ways there
 * or to stay. */
function Passed({ at, stay }: { at: At; stay: (stop: number) => void }) {
  const { state, look } = useFocus();
  const paths = useLinkPaths();
  const go = useGo();
  const inferior = state.inferior;
  const shown = state.stops.find((candidate) => candidate.stop === at.stop);
  const now = inferior.state === "stopped" ? inferior : null;
  const thread = now && state.threads.find((candidate) => candidate.id === now.thread);

  const goToNow = useCallback(() => {
    if (!now) {
      return;
    }
    tab.setState({ pinned: false });
    go({ stop: now.stop, thread: now.thread, frame: now.frame });
  }, [go, now]);
  const goToSession = useCallback(() => go(null), [go]);
  const stayHere = useCallback(() => {
    stay(at.stop);
    const place = shown?.place;
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
  }, [stay, at, shown, look, go, paths]);

  return (
    // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a notice, not a form's result
    <div className="stop-banner" role="status" data-testid="passed">
      <div>
        <b>
          {unreached(state, at.stop)
            ? `This session has not reached stop #${at.stop}.`
            : `Stop #${at.stop} has passed.`}
        </b>{" "}
        {now
          ? `The session is at stop #${now.stop}, ${placeText(now.place)} in ${threadLabel(now.thread, thread?.name)}.`
          : inferior.state === "running"
            ? "The program is running."
            : "The program is no longer stopped."}{" "}
        <span className="muted">Values are hidden: they belong to stop #{at.stop}.</span>
      </div>
      <div className="actions">
        {now ? (
          <button type="button" className="button primary small" onClick={goToNow}>
            Go to stop #{now.stop}
          </button>
        ) : (
          <button type="button" className="button small" onClick={goToSession}>
            Go to the session
          </button>
        )}
        {shown?.place?.path && shown.place.line && (
          <button type="button" className="button small" onClick={stayHere}>
            Stay on {placeText(shown.place)}
          </button>
        )}
      </div>
    </div>
  );
}
