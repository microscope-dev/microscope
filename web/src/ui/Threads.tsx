import { useCallback } from "react";

import { assert } from "../assert";
import { openingFrame } from "../follow";
import type { Inferior, Thread } from "../protocol";
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
  const choose = useCallback(
    (thread: number) => {
      assert(at !== null, `thread ${thread} chosen with no stop`);
      go({ stop: at.stop, thread, frame: openingFrame(state, at.stop, thread) });
    },
    [at, go, state],
  );

  return (
    <div className={`pane-body ${stale ? "stale" : ""}`}>
      {state.threads.length === 0 ? (
        <div className="empty">{emptyThreads(inferior)}</div>
      ) : (
        <ul className="rows" data-testid="threads">
          {state.threads.map((thread) => (
            <ThreadRow
              key={thread.id}
              thread={thread}
              shown={thread.id === shown}
              stopping={thread.id === stopping}
              choosable={choosable}
              onChoose={choose}
            />
          ))}
        </ul>
      )}
    </div>
  );
}

function ThreadRow({
  thread,
  shown,
  stopping,
  choosable,
  onChoose,
}: {
  thread: Thread;
  shown: boolean;
  stopping: boolean;
  choosable: boolean;
  onChoose: (thread: number) => void;
}) {
  const onClick = useCallback(() => onChoose(thread.id), [onChoose, thread.id]);
  return (
    <li>
      <button
        type="button"
        className={`row button-row ${shown ? "selected" : ""}`}
        disabled={!choosable}
        aria-current={shown ? "true" : undefined}
        onClick={onClick}
      >
        <span>{threadLabel(thread.id, thread.name)}</span>
        {thread.labels.map((label) => (
          <span key={label} className="dim">
            {label}
          </span>
        ))}
        <span className="end">
          {stopping ? "◂ stopped here" : thread.stopped ? "stopped" : "running"}
        </span>
      </button>
    </li>
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
