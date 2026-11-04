import { useNavigate } from "@tanstack/react-router";
import { useCallback, useEffect, useState } from "react";

import { assert } from "../assert";
import { stopPath } from "../focus";
import { stopKind } from "../model";
import type { StopEntry } from "../protocol";
import { useModel } from "../store";
import { useTab } from "../tab";
import { placeText } from "./Banner";

/** The most stops the strip lists. */
const SHOWN = 8;

/** How long a flash message stays. */
const FLASH = 4000;

/**
 * The status bar: the session's latest stops, clickable, and what others
 * did or what the tab just did.
 */
export function StatusBar() {
  const notices = useModel((model) => model.notices);
  const me = useModel((model) => model.hello?.connection);
  const state = useModel((model) => model.state);
  const flash = useTab((current) => current.flash);
  const now = useNow(flash ? flash.at + FLASH : null);
  const latest = notices.findLast((notice) => notice.connection !== me);
  const stops = state?.session ? state.stops.slice(-SHOWN) : [];

  return (
    <footer className="strip status-bar">
      {stops.length > 0 && (
        <nav className="stops" aria-label="Stops" data-testid="stops">
          <span className="muted">Stops</span>
          {stops.map((entry) => (
            <StopButton key={entry.stop} entry={entry} className="stop" />
          ))}
        </nav>
      )}
      {/* Only what was just said is read out, not the stops on every stop. */}
      <span className="end" aria-live="polite">
        {flash && now - flash.at < FLASH ? (
          <span className="notice" data-testid="flash">
            {flash.text}
          </span>
        ) : latest ? (
          <span className="notice" data-testid="notice">
            <b>{latest.name}</b> {latest.text}{" "}
            <span className="muted">{new Date(latest.at).toLocaleTimeString()}</span>
          </span>
        ) : null}
      </span>
    </footer>
  );
}

/** Every stop the session made, the latest first. */
export function Stops() {
  const stops = useModel((model) => model.state?.stops);
  if (!stops || stops.length === 0) {
    return (
      <div className="pane-body">
        <div className="empty">Stops appear here as the program stops.</div>
      </div>
    );
  }
  return (
    <div className="pane-body">
      <ul className="rows" data-testid="stop-list">
        {stops.toReversed().map((entry) => (
          <li key={entry.stop}>
            <StopButton entry={entry} className="row button-row" />
          </li>
        ))}
      </ul>
    </div>
  );
}

/** A stop, which shows the tab that stop when chosen. */
function StopButton({ entry, className }: { entry: StopEntry; className: string }) {
  const session = useModel((model) => model.state?.session);
  const current = useModel((model) =>
    model.state?.inferior.state === "stopped" ? model.state.inferior.stop : null,
  );
  const at = useTab((state) => state.at);
  const navigate = useNavigate();
  const on = entry.stop === at?.stop;
  const onClick = useCallback(() => {
    assert(session, `stop ${entry.stop} listed with no session`);
    void navigate({
      href: stopPath(session, { stop: entry.stop, thread: entry.thread, frame: entry.frame }),
    });
  }, [session, navigate, entry]);
  return (
    <button
      type="button"
      className={`${className} ${on ? "on" : ""} ${entry.stop === current ? "current" : ""}`}
      aria-current={on ? "true" : undefined}
      title={describeStop(entry)}
      onClick={onClick}
    >
      #{entry.stop} {stopKind(entry.reason.kind)} · {placeText(entry.place)}
      {entry.stop === current && " ◂ now"}
    </button>
  );
}

function describeStop(entry: StopEntry): string {
  const who = entry.by ? `${entry.by} ${entry.action ?? "ran the program"} · ` : "";
  return `#${entry.stop} · ${who}${entry.reason.description} · ${placeText(entry.place)}`;
}

/** The time, brought up to date again at `deadline`. */
function useNow(deadline: number | null): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (deadline === null) {
      return;
    }
    const timer = setTimeout(() => setNow(Date.now()), Math.max(0, deadline - Date.now()));
    return () => clearTimeout(timer);
  }, [deadline]);
  return now;
}
