import { useCallback, useState } from "react";

import { assert } from "../assert";
import { unreached } from "../follow";
import type { Backtrace, Frame } from "../protocol";
import { stackRows } from "../stack";
import { threadLabel } from "./names";
import { useGo } from "./navigation";
import { fileName } from "./paths";
import { useFocus } from "./Workspace";

const NONE_OPEN: ReadonlySet<number> = new Set();

/** The call stack of the thread or task shown; choosing a frame moves
 * every pane. */
export function Stack() {
  const { state, at, trace, stale } = useFocus();
  const go = useGo();

  // The runs opened in this stack; another stack folds afresh.
  const [opened, setOpened] = useState<{
    trace?: Backtrace | undefined;
    runs: ReadonlySet<number>;
  }>({
    runs: new Set(),
  });
  const runs = opened.trace === trace ? opened.runs : NONE_OPEN;
  const toggle = useCallback(
    (first: number) => {
      const next = new Set(runs);
      if (!next.delete(first)) {
        next.add(first);
      }
      setOpened({ trace, runs: next });
    },
    [runs, trace],
  );
  const choose = useCallback(
    (frame: number) => {
      assert(at !== null, `frame ${frame} chosen with no stop`);
      go({ ...at, frame });
    },
    [at, go],
  );

  let body: React.ReactNode;
  if (!at) {
    body = <div className="empty">The stack appears when the program stops.</div>;
  } else if (trace) {
    body = (
      <ul className="rows" data-testid="stack">
        {stackRows(trace, at.frame, runs).map((row) => {
          if (row.kind === "fold") {
            const { first, count } = row.run;
            assert(count > 0, `fold at frame ${first} holds ${count} frames`);
            return (
              <FoldRow
                key={`fold ${first}`}
                first={first}
                count={count}
                open={row.open}
                onToggle={toggle}
              />
            );
          }
          const { frame } = row;
          return [
            frame.unfollowed && (
              <li key={`${frame.index} unfollowed`} className="row dim" title={frame.unfollowed}>
                {frame.unfollowed}
              </li>
            ),
            <FrameRow
              key={frame.index}
              frame={frame}
              selected={frame.index === at.frame}
              onChoose={choose}
            />,
          ];
        })}
        {trace.incomplete && (
          <li className="row dim" title={trace.incomplete}>
            the stack ends here: {trace.incomplete}
          </li>
        )}
      </ul>
    );
  } else {
    body = (
      <div className="empty">
        {stale === "passed"
          ? unreached(state, at.stop)
            ? `This session has not reached stop #${at.stop}.`
            : `Stop #${at.stop} has passed, so its stack is no longer available.`
          : "Reading the stack…"}
      </div>
    );
  }
  return <div className={`pane-body ${stale && stale !== "passed" ? "stale" : ""}`}>{body}</div>;
}

function FoldRow({
  first,
  count,
  open,
  onToggle,
}: {
  first: number;
  count: number;
  open: boolean;
  onToggle: (first: number) => void;
}) {
  const onClick = useCallback(() => onToggle(first), [onToggle, first]);
  return (
    <li>
      <button
        type="button"
        className="row button-row fold"
        aria-expanded={open}
        title={`Frames ${first}–${first + count - 1}: the runtime's own`}
        onClick={onClick}
      >
        <span className="dim index">{open ? "▾" : "▸"}</span>
        <span className="dim">
          {count} frames of the runtime{open ? "" : " folded"}
        </span>
      </button>
    </li>
  );
}

function FrameRow({
  frame,
  selected,
  onChoose,
}: {
  frame: Frame;
  selected: boolean;
  onChoose: (frame: number) => void;
}) {
  const onClick = useCallback(() => onChoose(frame.index), [onChoose, frame.index]);
  return (
    <li>
      <button
        type="button"
        className={`row button-row ${selected ? "selected" : ""}`}
        aria-current={selected ? "true" : undefined}
        title={frame.address ? `${frame.name} at ${frame.address}` : frame.name}
        onClick={onClick}
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
    </li>
  );
}

/** Whose stack the tab shows: the task's, or the thread's. */
export function StackBadge() {
  const { state, at } = useFocus();
  if (at?.task) {
    return <span className="count">task {at.task.number}</span>;
  }
  const thread = state.threads.find((candidate) => candidate.id === at?.thread);
  return thread ? <span className="count">{threadLabel(thread.id, thread.name)}</span> : null;
}

/**
 * Where a frame's code is: its file, line, and column where that is known,
 * or its module or address.
 */
export function where(frame: Frame): string {
  if (frame.source) {
    const column = frame.source.column === null ? "" : `:${frame.source.column}`;
    return `${fileName(frame.source.path)}:${frame.source.line}${column}`;
  }
  return frame.module ?? frame.address ?? "";
}
