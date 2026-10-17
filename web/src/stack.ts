// The rows a call stack shows: its frames, with the runs of a runtime's or
// an interpreter's machinery folded into a row each, as the CLI's `bt`
// folds them.

import type { Backtrace, Frame, FrameRun } from "./protocol";

export type StackRow =
  | { kind: "frame"; frame: Frame }
  | { kind: "fold"; run: FrameRun; open: boolean };

/** The rows of `trace`, with the runs whose first frames `opened` holds
 * shown, and the run that holds the `selected` frame shown too. */
export function stackRows(
  trace: Backtrace,
  selected: number | undefined,
  opened: ReadonlySet<number>,
): StackRow[] {
  const rows: StackRow[] = [];
  const shown = (frames: Frame[]) => {
    for (const frame of frames) {
      rows.push({ kind: "frame", frame });
    }
  };
  let index = 0;
  for (const run of trace.folds) {
    shown(trace.frames.slice(index, run.first));
    const frames = trace.frames.slice(run.first, run.first + run.count);
    const open = opened.has(run.first) || frames.some((frame) => frame.index === selected);
    rows.push({ kind: "fold", run, open });
    if (open) {
      shown(frames);
    }
    index = run.first + run.count;
  }
  shown(trace.frames.slice(index));
  return rows;
}
