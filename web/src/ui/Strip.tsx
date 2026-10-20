import { useNavigate } from "@tanstack/react-router";
import { useEffect, useState } from "react";
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
 * The bottom strip: the session's latest stops, clickable, and what others
 * did or what the tab just did.
 */
export function Strip() {
  const notices = useModel((model) => model.notices);
  const me = useModel((model) => model.hello?.connection);
  const state = useModel((model) => model.state);
  const at = useTab((current) => current.at);
  const flash = useTab((current) => current.flash);
  const navigate = useNavigate();
  const now = useNow(flash ? flash.at + FLASH : null);
  const latest = [...notices].reverse().find((notice) => notice.connection !== me);
  const stops = state?.session ? state.stops.slice(-SHOWN) : [];
  const session = state?.session;
  const current = state?.inferior.state === "stopped" ? state.inferior.stop : null;

  return (
    <footer className="strip">
      {stops.length > 0 && (
        <nav className="stops" aria-label="Stops" data-testid="stops">
          <span className="muted">Stops</span>
          {stops.map((entry) => (
            <button
              key={entry.stop}
              type="button"
              className={`stop ${entry.stop === at?.stop ? "on" : ""} ${entry.stop === current ? "current" : ""}`}
              aria-current={entry.stop === at?.stop ? "true" : undefined}
              title={describeStop(entry)}
              onClick={() =>
                session &&
                navigate({
                  href: stopPath(session, {
                    stop: entry.stop,
                    thread: entry.thread,
                    frame: entry.frame,
                  }),
                })
              }
            >
              #{entry.stop} {stopKind(entry.reason.kind)} · {placeText(entry.place)}
              {entry.stop === current && " ◂ now"}
            </button>
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

function describeStop(entry: StopEntry): string {
  const who = entry.by ? `${entry.by} ${entry.action ?? "ran the program"} · ` : "";
  return `#${entry.stop} · ${who}${entry.reason.description} · ${placeText(entry.place)}`;
}

/** The time, brought up to date again at `deadline`. */
function useNow(deadline: number | null): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    setNow(Date.now());
    if (deadline === null) {
      return;
    }
    const timer = setTimeout(() => setNow(Date.now()), Math.max(0, deadline - Date.now()));
    return () => clearTimeout(timer);
  }, [deadline]);
  return now;
}
