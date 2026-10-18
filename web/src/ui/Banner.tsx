import { useState } from "react";
import { formatPlace } from "../focus";
import { latestStop } from "../follow";
import type { Place, StopEntry } from "../protocol";
import { tab, useTab } from "../tab";
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
  const { state, at, stale, look } = useFocus();
  const pinned = useTab((current) => current.pinned);
  const paths = useLinkPaths();
  const go = useGo();
  const [stayed, setStayed] = useState<number | null>(null);
  if (!at) {
    return null;
  }
  const inferior = state.inferior;
  const entry = (stop: number): StopEntry | undefined =>
    state.stops.find((candidate) => candidate.stop === stop);
  const shown = entry(at.stop);
  // A link can name a stop beyond any this session has made.
  const unreached = !shown && at.stop > (latestStop(state) ?? 0);

  if (stale === "passed" && stayed !== at.stop) {
    const now = inferior.state === "stopped" ? inferior : null;
    const thread = now && state.threads.find((candidate) => candidate.id === now.thread);
    return (
      <div className="stop-banner" role="status" data-testid="passed">
        <div>
          <b>
            {unreached
              ? `This session has not reached stop #${at.stop}.`
              : `Stop #${at.stop} has passed.`}
          </b>{" "}
          {now
            ? `The session is at stop #${now.stop}, ${placeText(now.place)} in ${thread?.name ?? `thread ${now.thread}`}.`
            : inferior.state === "running"
              ? "The program is running."
              : "The program is no longer stopped."}{" "}
          <span className="muted">Values are hidden: they belong to stop #{at.stop}.</span>
        </div>
        <div className="actions">
          {now ? (
            <button
              type="button"
              className="button primary small"
              onClick={() => {
                tab.setState({ pinned: false });
                go({ stop: now.stop, thread: now.thread, frame: now.frame });
              }}
            >
              Go to stop #{now.stop}
            </button>
          ) : (
            <button type="button" className="button small" onClick={() => go(null)}>
              Go to the session
            </button>
          )}
          {shown?.place?.path && shown.place.line && (
            <button
              type="button"
              className="button small"
              onClick={() => {
                setStayed(at.stop);
                const place = shown.place;
                if (place?.path && place.line && !look.src) {
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
              }}
            >
              Stay on {placeText(shown.place)}
            </button>
          )}
        </div>
      </div>
    );
  }
  if (pinned && stale !== "passed") {
    return (
      <div className="stop-banner quiet" role="status" data-testid="pinned">
        <div>Pinned to stop #{at.stop}: this tab stays here when the program stops elsewhere.</div>
        <button
          type="button"
          className="button small"
          onClick={() => tab.setState({ pinned: false })}
        >
          Unpin <kbd>p</kbd>
        </button>
      </div>
    );
  }
  return null;
}
