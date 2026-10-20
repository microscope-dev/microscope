import { useEffect, useMemo, useRef } from "react";
import { rangeOf } from "../../profile/look";
import type { SyscallQuery } from "../../protocol";
import { useShownScheme } from "../../theme";
import { NAMES, prepare, shade, useRangeDrag, useWidth } from "./canvas";
import { token, useAnswer, useProfile } from "./context";

const LANE = 18;

/** Diagonal strokes of `color`, which mark a thread blocked in a call. */
function hatching(color: string): CanvasPattern | null {
  const tile = document.createElement("canvas");
  tile.width = 6;
  tile.height = 6;
  const context = tile.getContext("2d");
  if (!context) {
    return null;
  }
  context.strokeStyle = color;
  context.globalAlpha = 0.45;
  context.beginPath();
  context.moveTo(0, 6);
  context.lineTo(6, 0);
  context.stroke();
  return context.createPattern(tile, "repeat");
}

/** A lane per thread of its time in calls over the whole profile, with
 * failed calls as red ticks, under the samples' timeline on one clock. */
export function Lanes({ query }: { query: SyscallQuery }) {
  const { summary, look } = useProfile();
  const [box, width] = useWidth();
  const canvas = useRef<HTMLCanvasElement>(null);
  const plot = Math.max(width - NAMES, 0);
  const buckets = Math.max(1, Math.min(2048, Math.floor(plot / 2)));
  const span = Math.max(summary.duration, 1);
  const scheme = useShownScheme();
  const { range: _range, ...whole } = query;
  const answer = useAnswer("syscallLanes", plot > 0 ? { ...whole, buckets } : null);
  const lanes = answer.data?.lanes ?? [];
  const height = Math.max(lanes.length, 1) * LANE;
  const range = useMemo(() => rangeOf({ range: look.range }), [look.range]);
  const { handlers, shown } = useRangeDrag(span);

  useEffect(() => {
    const context = prepare(canvas.current, plot, height);
    const data = answer.data;
    if (!context || !data) {
      return;
    }
    canvas.current?.setAttribute("data-scheme", scheme);
    const slice = (plot * data.width) / span;
    const x = (bucket: number) => (plot * (data.start + bucket * data.width)) / span;
    const busy = token("--accent");
    const failed = token("--error");
    const hatch = hatching(busy);
    for (const [index, lane] of data.lanes.entries()) {
      const top = index * LANE;
      // Blocked: one call spans the whole slice.
      if (hatch) {
        context.fillStyle = hatch;
        for (const [bucket, blocked] of lane.blocked.entries()) {
          if (blocked) {
            context.fillRect(x(bucket), top + 1, Math.max(slice, 1), LANE - 2);
          }
        }
      }
      context.fillStyle = busy;
      for (const [bucket, nanoseconds] of lane.busy.entries()) {
        if (nanoseconds > 0) {
          // As tall as the share of its slice the thread spent in calls.
          const h = Math.max(1, (LANE - 3) * Math.min(1, nanoseconds / data.width));
          context.globalAlpha = 0.75;
          context.fillRect(x(bucket), top + LANE - 1 - h, Math.max(slice, 1), h);
        }
      }
      context.globalAlpha = 1;
      context.fillStyle = failed;
      for (const [bucket, count] of lane.failed.entries()) {
        if (count > 0) {
          context.fillRect(x(bucket), top + 1, Math.max(slice, 2), 4);
        }
      }
    }
    shade(context, shown ?? range, span, plot, height);
  }, [answer.data, plot, height, span, range, shown, scheme]);

  return (
    <div className="profile-timeline syscall-lanes" ref={box} data-testid="syscall-lanes">
      <div className="lane-names" style={{ width: NAMES }}>
        {lanes.map((lane) => (
          <span
            key={`${lane.pid}-${lane.tid}`}
            className="lane-name"
            title={`pid ${lane.pid}, tid ${lane.tid}`}
          >
            {lane.name ?? "thread"} <span className="dim">{lane.tid}</span>
          </span>
        ))}
      </div>
      {answer.error ? (
        <div className="empty error" style={{ width: plot }}>
          {answer.error.message}
        </div>
      ) : (
        <canvas
          ref={canvas}
          className="lanes"
          style={{ width: plot, height }}
          role="img"
          aria-label="Each thread's time in system calls"
          {...handlers}
        />
      )}
    </div>
  );
}
