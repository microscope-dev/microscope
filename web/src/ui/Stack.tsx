import { useState } from "react";
import type { Backtrace, Frame } from "../protocol";
import { stackRows } from "../stack";
import { useGo } from "./navigation";
import { fileName } from "./paths";
import { useFocus } from "./Workspace";

const NONE_OPEN: ReadonlySet<number> = new Set();

/** The call stack of the thread or task shown; choosing a frame moves
 * every pane. */
export function Stack() {
  const { state, at, trace, stale } = useFocus();
  const go = useGo();
  const thread = state.threads.find((candidate) => candidate.id === at?.thread);

  // The runs opened in this stack; another stack folds afresh.
  const [opened, setOpened] = useState<{
    trace?: Backtrace | undefined;
    runs: ReadonlySet<number>;
  }>({
    runs: new Set(),
  });
  const runs = opened.trace === trace ? opened.runs : NONE_OPEN;
  const toggle = (first: number) => {
    const next = new Set(runs);
    if (!next.delete(first)) {
      next.add(first);
    }
    setOpened({ trace, runs: next });
  };

  let body: React.ReactNode;
  if (!at) {
    body = <div className="empty">The stack appears when the program stops.</div>;
  } else if (!trace) {
    body = (
      <div className="empty">
        {stale === "passed"
          ? `Stop #${at.stop} has passed; its stack is gone with it.`
          : "Reading the stack…"}
      </div>
    );
  } else {
    body = (
      <ul className="rows" data-testid="stack">
        {stackRows(trace, at.frame, runs).map((row) => {
          if (row.kind === "fold") {
            const { first, count } = row.run;
            return (
              <li key={`fold ${first}`}>
                <button
                  type="button"
                  className="row button-row fold"
                  aria-expanded={row.open}
                  title={`Frames ${first}–${first + count - 1}: the runtime's own`}
                  onClick={() => toggle(first)}
                >
                  <span className="dim index">{row.open ? "▾" : "▸"}</span>
                  <span className="dim">
                    {count} frames of the runtime{row.open ? "" : " folded"}
                  </span>
                </button>
              </li>
            );
          }
          const { frame } = row;
          return [
            frame.unfollowed && (
              <li key={`${frame.index} unfollowed`} className="row dim" title={frame.unfollowed}>
                {frame.unfollowed}
              </li>
            ),
            <li key={frame.index}>
              <button
                type="button"
                className={`row button-row ${frame.index === at.frame ? "selected" : ""}`}
                aria-current={frame.index === at.frame ? "true" : undefined}
                title={frame.address ? `${frame.name} at ${frame.address}` : frame.name}
                onClick={() => go({ ...at, frame: frame.index })}
              >
                <span className="dim index">{frame.index}</span>
                <span className="mark" title={frame.language?.name}>
                  {frame.language?.mark}
                </span>
                <span className={frame.source ? "fn" : "fn subtle"}>
                  {frame.name}
                  {frame.kind === "inline" && <span className="dim"> inlined</span>}
                  {frame.kind === "tailCall" && <span className="dim"> tail call</span>}
                  {frame.kind === "async" && <span className="dim"> async</span>}
                </span>
                <span className="end">{where(frame)}</span>
              </button>
            </li>,
          ];
        })}
        {trace.incomplete && (
          <li className="row dim" title={trace.incomplete}>
            the stack ends here: {trace.incomplete}
          </li>
        )}
      </ul>
    );
  }
  return (
    <section className="pane grow" aria-label="Call stack" data-pane="2" tabIndex={-1}>
      <div className="pane-head">
        Call stack
        {at?.task ? (
          <span className="count">task {at.task.number}</span>
        ) : (
          thread && <span className="count">{thread.name ?? thread.id}</span>
        )}
        <span className="end">Alt+2</span>
      </div>
      <div className={`pane-body ${stale && stale !== "passed" ? "stale" : ""}`}>{body}</div>
    </section>
  );
}

/** Where a frame's code is: its file and line, or its module or address. */
export function where(frame: Frame): string {
  if (frame.source) {
    return `${fileName(frame.source.path)}:${frame.source.line}`;
  }
  return frame.module ?? frame.address ?? "";
}
