import { useEffect, useMemo, useRef, useState } from "react";
import type { Requested } from "../../data";
import { rangeOf, withLook } from "../../profile/look";
import { formatCount, largest } from "../../profile/paint";
import { seriesColor, stacked } from "../../profile/syscalls";
import type { SyscallGroup, SyscallQuery, SyscallRow, SyscallSummary } from "../../protocol";
import { useShownScheme } from "../../theme";
import { NAMES, prepare, shade, useRangeDrag, useWidth } from "./canvas";
import { token, useAnswer, useProfile } from "./context";
import { Bytes, Duration } from "./parts";
import { Detail } from "./SyscallDetail";

const CHART = 120;

const GROUPS: { group: SyscallGroup; label: string }[] = [
  { group: "syscall", label: "by call" },
  { group: "class", label: "by class" },
  { group: "process", label: "by process" },
  { group: "thread", label: "by thread" },
  { group: "path", label: "by file" },
  { group: "site", label: "by call site" },
];

/** The rate chart, the table, and the selected call's detail. */
export function Summary({
  summary,
  query,
}: {
  summary: Requested<SyscallSummary>;
  query: SyscallQuery;
}) {
  const { look, setLook } = useProfile();
  const group = look.sg ?? "syscall";
  return (
    <div className="syscalls-summary">
      <Rate query={query} />
      <div className="syscalls-split">
        <div className="pane syscalls-table-pane">
          <div className="pane-head">
            <select
              className="input profile-by"
              aria-label="Group"
              value={group}
              onChange={(event) => {
                const group = event.target.value as SyscallGroup;
                setLook((look) =>
                  withLook(
                    withLook(look, "sc", undefined),
                    "sg",
                    group === "syscall" ? undefined : group,
                  ),
                );
              }}
            >
              {GROUPS.map(({ group, label }) => (
                <option key={group} value={group}>
                  {label}
                </option>
              ))}
            </select>
          </div>
          {summary.data ? (
            <Table rows={summary.data.rows} selectable={group === "syscall"} />
          ) : summary.error ? null : (
            // The selection's error shows beside it.
            <p className="muted pad">Reading the summary…</p>
          )}
        </div>
        {look.sc && group === "syscall" ? <Detail call={look.sc} query={query} /> : null}
      </div>
    </div>
  );
}

type Sort = "total" | "calls" | "errors" | "p99" | "name";

/** Every row with `-c`'s columns and percentiles, sortable; a call's row
 * selects it. */
function Table({ rows, selectable }: { rows: SyscallRow[]; selectable: boolean }) {
  const { look, setLook } = useProfile();
  const [sort, setSort] = useState<Sort>("total");
  const [open, setOpen] = useState<string | null>(null);
  const sorted = useMemo(() => {
    const key = (row: SyscallRow): number | string =>
      sort === "name"
        ? row.name
        : sort === "calls"
          ? row.calls
          : sort === "errors"
            ? row.errors
            : sort === "p99"
              ? (row.p99Ns ?? 0)
              : row.totalNs;
    return [...rows].sort((a, b) => {
      const [x, y] = [key(a), key(b)];
      return typeof x === "string" ? x.localeCompare(String(y)) : Number(y) - Number(x);
    });
  }, [rows, sort]);
  const header = (column: Sort, label: string, numeric = true) => (
    <th
      className={numeric ? "num" : undefined}
      aria-sort={sort === column ? (column === "name" ? "ascending" : "descending") : undefined}
    >
      <button type="button" className="sort" onClick={() => setSort(column)}>
        {label}
      </button>
    </th>
  );
  return (
    <table className="syscalls-table" data-testid="syscall-table">
      <thead>
        <tr>
          {header("name", "name", false)}
          {header("calls", "calls")}
          {header("errors", "errors")}
          {header("total", "total")}
          <th className="num">p50</th>
          <th className="num">p90</th>
          {header("p99", "p99")}
          <th className="num">max</th>
          <th className="num">bytes</th>
        </tr>
      </thead>
      <tbody>
        {sorted.map((row) => (
          <SummaryRow
            key={row.key}
            row={row}
            selected={selectable && look.sc === row.key}
            open={open === row.key}
            toggle={() => setOpen(open === row.key ? null : row.key)}
            select={
              selectable
                ? () =>
                    setLook((look) =>
                      withLook(look, "sc", look.sc === row.key ? undefined : row.key),
                    )
                : undefined
            }
          />
        ))}
      </tbody>
    </table>
  );
}

function SummaryRow({
  row,
  selected,
  open,
  toggle,
  select,
}: {
  row: SyscallRow;
  selected: boolean;
  open: boolean;
  toggle(): void;
  select: (() => void) | undefined;
}) {
  const time = (value: number | null) => (value === null ? "–" : <Duration ns={value} />);
  return (
    <>
      <tr className={selected ? "selected" : undefined} onClick={select} data-row={row.key}>
        <td className="mono name" title={row.name}>
          {row.errnos.length > 0 ? (
            <button
              type="button"
              className="disclose"
              aria-expanded={open}
              aria-label="Errors by errno"
              onClick={(event) => {
                event.stopPropagation();
                toggle();
              }}
            >
              {open ? "▾" : "▸"}
            </button>
          ) : null}
          {select ? (
            // The whole row selects for a pointer; the name does for a keyboard.
            <button
              type="button"
              className="link-button"
              aria-pressed={selected}
              onClick={(event) => {
                event.stopPropagation();
                select();
              }}
            >
              {row.name}
            </button>
          ) : (
            row.name
          )}
        </td>
        <td className="num">{formatCount(row.calls)}</td>
        <td className={`num ${row.errors > 0 ? "error-text" : ""}`}>{formatCount(row.errors)}</td>
        <td className="num">
          <Duration ns={row.totalNs} />
        </td>
        <td className="num">{time(row.p50Ns)}</td>
        <td className="num">{time(row.p90Ns)}</td>
        <td className="num">{time(row.p99Ns)}</td>
        <td className="num">{row.timed > 0 ? <Duration ns={row.maxNs} /> : "–"}</td>
        <td className="num">{row.bytes > 0 ? <Bytes bytes={row.bytes} /> : ""}</td>
      </tr>
      {open
        ? row.errnos.map((errno) => (
            <tr key={errno.name} className="errno-row">
              <td className="mono error-text">{errno.name}</td>
              <td className="num">{formatCount(errno.count)}</td>
              <td colSpan={7} />
            </tr>
          ))
        : null}
    </>
  );
}

/** Calls per second over the whole profile, stacked by class or by call,
 * with failures as a red line; dragging selects a range. */
function Rate({ query }: { query: SyscallQuery }) {
  const { summary, look, setLook } = useProfile();
  const [box, width] = useWidth();
  const canvas = useRef<HTMLCanvasElement>(null);
  const plot = Math.max(width - NAMES, 0);
  const buckets = Math.max(1, Math.min(1024, Math.floor(plot / 4)));
  const span = Math.max(summary.duration, 1);
  const scheme = useShownScheme();
  const { range: _range, ...whole } = query;
  const by = look.sr ?? "class";
  const answer = useAnswer(
    "syscallRate",
    plot > 0 ? { ...whole, buckets, by: by === "syscall" ? "syscall" : "class" } : null,
  );
  const range = useMemo(() => rangeOf({ range: look.range }), [look.range]);
  const { handlers, shown } = useRangeDrag(span);
  const series = answer.data?.series ?? [];

  useEffect(() => {
    const context = prepare(canvas.current, plot, CHART);
    const data = answer.data;
    if (!context || !data) {
      return;
    }
    canvas.current?.setAttribute("data-scheme", scheme);
    const tops = stacked(data.series.map((series) => series.counts));
    const most = largest(tops.at(-1) ?? [], 1);
    const slice = (plot * data.width) / span;
    const x = (bucket: number) => (plot * (data.start + bucket * data.width)) / span;
    const y = (count: number) => CHART - 2 - ((CHART - 6) * count) / most;
    const dark = scheme === "dark";
    for (const [index, series] of data.series.entries()) {
      context.fillStyle = seriesColor(series.name, dark);
      for (const [bucket, count] of series.counts.entries()) {
        if (count > 0) {
          const top = tops[index]?.[bucket] ?? 0;
          context.fillRect(x(bucket), y(top), Math.max(slice, 1), y(top - count) - y(top));
        }
      }
    }
    context.strokeStyle = token("--error");
    context.lineWidth = 1.5;
    context.beginPath();
    for (const [bucket, count] of data.errors.entries()) {
      const at = x(bucket) + slice / 2;
      if (bucket === 0) {
        context.moveTo(at, y(count));
      } else {
        context.lineTo(at, y(count));
      }
    }
    if (data.errors.some((count) => count > 0)) {
      context.stroke();
    }
    shade(context, shown ?? range, span, plot, CHART);
  }, [answer.data, plot, span, range, shown, scheme]);

  const dark = scheme === "dark";
  return (
    <div className="syscalls-rate" ref={box} data-testid="syscall-rate">
      <div className="syscalls-legend" style={{ width: NAMES }}>
        <div className="segmented">
          {(["class", "syscall"] as const).map((each) => (
            <button
              key={each}
              type="button"
              aria-pressed={by === each}
              onClick={() =>
                setLook((look) => withLook(look, "sr", each === "class" ? undefined : "syscall"))
              }
            >
              {each === "class" ? "Classes" : "Calls"}
            </button>
          ))}
        </div>
        {series.map((series) => (
          <span key={series.name} className="legend-item">
            <span className="swatch" style={{ background: seriesColor(series.name, dark) }} />
            {by === "class" && series.name !== "other" ? `%${series.name}` : series.name}
          </span>
        ))}
        {answer.data?.errors.some((count) => count > 0) ? (
          <span className="legend-item error-text">― failed</span>
        ) : null}
      </div>
      {answer.error ? (
        <div className="empty error" style={{ width: plot }}>
          {answer.error.message}
        </div>
      ) : (
        <canvas
          ref={canvas}
          style={{ width: plot, height: CHART }}
          role="img"
          aria-label="Calls over time"
          {...handlers}
        />
      )}
    </div>
  );
}
