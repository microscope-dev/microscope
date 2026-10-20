import { useEffect, useMemo, useRef } from "react";
import { withLook } from "../../profile/look";
import { formatCount, formatDuration, largest } from "../../profile/paint";
import { bucketLabel, heatAlpha, shownBuckets } from "../../profile/syscalls";
import type { SyscallDetail, SyscallQuery } from "../../protocol";
import { useShownScheme } from "../../theme";
import { prepare, useWidth } from "./canvas";
import { token, useAnswer, useProfile } from "./context";
import { Unanswered } from "./parts";

/** One call's heat map of latency over the range, its histogram against
 * the whole trace's, its errors, and where its slow and failed calls came
 * from. */
export function Detail({ call, query }: { call: string; query: SyscallQuery }) {
  const selected = useMemo(
    () => ({ ...query, filter: `${query.filter} trace=${call}`.trim() }),
    [query, call],
  );
  const detail = useAnswer("syscallDetail", { ...query, call });
  const data = detail.data;
  return (
    <div className="pane syscalls-detail" data-testid="syscall-detail">
      <div className="pane-head">
        <span className="mono">{call}</span>
      </div>
      <Heatmap query={selected} call={call} />
      {detail.error || !data ? (
        <Unanswered error={detail.error} reading={`Reading ${call}'s calls…`} />
      ) : (
        <Histograms data={data} />
      )}
    </div>
  );
}

/** The call's latencies against the whole trace's, its errors, and its callers. */
function Histograms({ data }: { data: SyscallDetail }) {
  const [first, last] = shownBuckets(data.whole, data.selected);
  const most = largest(data.selected, largest(data.whole, 1));
  return (
    <>
      <section>
        <h3>Latency</h3>
        {last < first ? <p className="muted">No call was timed.</p> : null}
        <div className="latency-bars">
          {last >= first
            ? data.whole.slice(first, last + 1).map((whole, offset) => {
                const bucket = first + offset;
                const chosen = data.selected[bucket] ?? 0;
                return (
                  <div
                    key={bucket}
                    className="histogram-row"
                    title={`${formatCount(chosen)} of ${formatCount(whole, "call")}`}
                  >
                    <span className="mono muted">{bucketLabel(bucket)}</span>
                    <span className="histogram-bar">
                      <span className="whole" style={{ width: `${(100 * whole) / most}%` }} />
                      <span className="chosen" style={{ width: `${(100 * chosen) / most}%` }} />
                    </span>
                    <span className="num">{formatCount(chosen)}</span>
                  </div>
                );
              })
            : null}
        </div>
      </section>
      {data.errnos.length > 0 ? (
        <section>
          <h3>Errors</h3>
          {data.errnos.map((errno) => (
            <div key={errno.name} className="histogram-row">
              <span className="mono error-text">{errno.name}</span>
              <span className="histogram-bar">
                <span
                  className="error"
                  style={{
                    width: `${(100 * errno.count) / Math.max(1, data.errnos[0]?.count ?? 1)}%`,
                  }}
                />
              </span>
              <span className="num">{formatCount(errno.count)}</span>
            </div>
          ))}
        </section>
      ) : null}
      <section data-testid="syscall-callers">
        <h3>
          Callers of slow{data.slowNs !== null ? ` (≥ ${formatDuration(data.slowNs)})` : ""} and
          failed calls
        </h3>
        {data.callers.length === 0 ? <p className="muted">No stacks were recorded.</p> : null}
        {data.callers.map((caller) => (
          <div key={caller.site} className="caller">
            <span className="mono">{caller.site}</span>
            <span className="muted">
              {caller.slow > 0 ? `${formatCount(caller.slow)} slow` : ""}
              {caller.slow > 0 && caller.failed > 0 ? ", " : ""}
              {caller.failed > 0 ? (
                <span className="error-text">{formatCount(caller.failed)} failed</span>
              ) : null}
            </span>
          </div>
        ))}
        {data.unplaced > 0 ? (
          <p className="muted">{formatCount(data.unplaced)} had no stack.</p>
        ) : null}
      </section>
    </>
  );
}

/** Calls by when they returned, across, and how long they took, up. */
function Heatmap({ query, call }: { query: SyscallQuery; call: string }) {
  const { look, setLook } = useProfile();
  const [box, width] = useWidth();
  const canvas = useRef<HTMLCanvasElement>(null);
  const scheme = useShownScheme();
  const buckets = Math.max(1, Math.min(512, Math.floor(width / 4)));
  const answer = useAnswer("syscallHeatmap", width > 0 ? { ...query, buckets } : null);
  const data = answer.data;
  // Only the latencies that occur, so the cells stay tall.
  const [first, last] = useMemo(() => {
    if (!data) {
      return [0, -1];
    }
    const rows = Array.from({ length: data.rows }, (_, row) => {
      let count = 0;
      for (let slice = 0; slice * data.rows < data.counts.length; slice++) {
        count += data.counts[slice * data.rows + row] ?? 0;
      }
      return count;
    });
    return shownBuckets(rows);
  }, [data]);
  const height = 140;

  useEffect(() => {
    const context = prepare(canvas.current, width, height);
    if (!context || !data || last < first) {
      return;
    }
    canvas.current?.setAttribute("data-scheme", scheme);
    const shown = last - first + 1;
    const cell = height / shown;
    const slices = data.counts.length / data.rows;
    const column = width / slices;
    context.fillStyle = token("--accent");
    for (let slice = 0; slice < slices; slice++) {
      for (let row = first; row <= last; row++) {
        const alpha = heatAlpha(data.counts[slice * data.rows + row] ?? 0, data.most);
        if (alpha > 0) {
          context.globalAlpha = alpha;
          context.fillRect(
            slice * column,
            height - (row - first + 1) * cell,
            Math.max(column, 1),
            cell,
          );
        }
      }
    }
    context.globalAlpha = 1;
  }, [data, width, first, last, scheme]);

  return (
    <section className="latency-map" data-testid="syscall-heatmap">
      <h3>
        Latency over time
        {last >= first ? (
          <span className="muted">
            {" "}
            · {bucketLabel(first)} to {bucketLabel(last)}
          </span>
        ) : null}
      </h3>
      {/* Measured without the section's padding, and never wider than
          it, so the canvas cannot widen the column it is measured in. */}
      <div ref={box} className="plot">
        {answer.error ? (
          <div className="empty error">{answer.error.message}</div>
        ) : (
          <canvas
            ref={canvas}
            className="clickable"
            style={{ width, height }}
            role="img"
            aria-label="Latency heat map"
            title="A cell's calls, in the log"
            onClick={(event) => {
              if (!data || last < first) {
                return;
              }
              const box = event.currentTarget.getBoundingClientRect();
              const slices = data.counts.length / data.rows;
              const slice = Math.floor(((event.clientX - box.left) / box.width) * slices);
              const row =
                last - Math.floor(((event.clientY - box.top) / box.height) * (last - first + 1));
              if (!data.counts[slice * data.rows + row]) {
                return;
              }
              const start = data.start + slice * data.width;
              const filter = `${look.sf ?? ""} trace=${call}`.trim();
              setLook((look) => {
                let next = withLook(look, "range", `${start}-${start + data.width}`);
                next = withLook(next, "sl", String(row));
                next = withLook(next, "sf", filter);
                next = withLook(next, "ev", undefined);
                return withLook(next, "st", "log");
              }, true);
            }}
          />
        )}
      </div>
    </section>
  );
}
