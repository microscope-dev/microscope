import { openingFrame } from "../follow";
import type { Inferior } from "../protocol";
import { threadLabel } from "./names";
import { useGo } from "./navigation";
import { useFocus } from "./Workspace";

/** Every thread; choosing one shows its stack at the stop. */
export function Threads() {
  const { state, at, stale } = useFocus();
  const go = useGo();
  const inferior = state.inferior;
  const stopping = inferior.state === "stopped" ? inferior.thread : null;
  // A task's focus shows no thread's stack.
  const shown = at?.task ? null : (at?.thread ?? stopping);
  // Threads can be chosen at the stop the tab shows, while it lasts.
  const choosable = at !== null && stale === null;

  return (
    <section
      className="pane"
      style={{ flex: "0 1 auto", maxHeight: "30%" }}
      aria-label="Threads"
      data-pane="1"
      tabIndex={-1}
    >
      <div className="pane-head">
        Threads <span className="count">{state.threads.length}</span>
        <span className="end">Alt+1</span>
      </div>
      <div className={`pane-body ${stale ? "stale" : ""}`}>
        {state.threads.length === 0 ? (
          <div className="empty">{emptyThreads(inferior)}</div>
        ) : (
          <ul className="rows" data-testid="threads">
            {state.threads.map((thread) => (
              <li key={thread.id}>
                <button
                  type="button"
                  className={`row button-row ${thread.id === shown ? "selected" : ""}`}
                  disabled={!choosable}
                  aria-current={thread.id === shown ? "true" : undefined}
                  onClick={() =>
                    at &&
                    go({
                      stop: at.stop,
                      thread: thread.id,
                      frame: openingFrame(state, at.stop, thread.id),
                    })
                  }
                >
                  <span>{threadLabel(thread.id, thread.name)}</span>
                  {thread.labels.map((label) => (
                    <span key={label} className="dim">
                      {label}
                    </span>
                  ))}
                  <span className="end">
                    {thread.id === stopping
                      ? "◂ stopped here"
                      : thread.stopped
                        ? "stopped"
                        : "running"}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </section>
  );
}

function emptyThreads(inferior: Inferior): string {
  switch (inferior.state) {
    case "notStarted":
      return "No threads until the program runs.";
    case "running":
      return "No threads yet.";
    case "stopped":
      return "No threads.";
    case "exited":
      return "The program exited.";
    case "detached":
      return "microscope let the process go.";
  }
}
