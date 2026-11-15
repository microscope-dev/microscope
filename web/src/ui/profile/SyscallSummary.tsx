import { type ChangeEvent, type MouseEvent, useCallback, useMemo, useState } from "react";

import type { Requested } from "../../data";
import { formatDuringRow, withLook } from "../../profile/look";
import { formatCount } from "../../profile/paint";
import { sampledKernel } from "../../profile/syscalls";
import type { SyscallGroup, SyscallQuery, SyscallRow, SyscallSummary } from "../../protocol";
import { useProfile } from "./context";
import { Bytes, Duration } from "./parts";
import { Detail } from "./SyscallDetail";

const GROUPS: { group: SyscallGroup; label: string }[] = [
  { group: "syscall", label: "by call" },
  { group: "class", label: "by class" },
  { group: "process", label: "by process" },
  { group: "thread", label: "by thread" },
  { group: "path", label: "by file" },
  { group: "site", label: "by call site" },
];

export function Summary({
  summary,
  query,
}: {
  summary: Requested<SyscallSummary>;
  query: SyscallQuery;
}) {
  const { look, setLook } = useProfile();
  const group = look.sg ?? "syscall";
  const regroup = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => {
      const chosen = event.target.value as SyscallGroup;
      setLook((prev) =>
        withLook(
          // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
          withLook(prev, "sc", undefined),
          "sg",
          chosen === "syscall" ? undefined : chosen,
        ),
      );
    },
    [setLook],
  );
  return (
    <div className="syscalls-summary">
      <div className="syscalls-split">
        <div className="pane syscalls-table-pane">
          <div className="pane-head">
            <select
              className="input profile-by"
              aria-label="Group"
              value={group}
              onChange={regroup}
            >
              {GROUPS.map(({ group: each, label }) => (
                <option key={each} value={each}>
                  {label}
                </option>
              ))}
            </select>
          </div>
          {summary.data ? (
            <Table rows={summary.data.rows} group={group} />
          ) : summary.error ? null : (
            <p className="muted pad">Reading the summary…</p>
          )}
        </div>
        {look.sc && group === "syscall" ? <Detail call={look.sc} query={query} /> : null}
      </div>
    </div>
  );
}

type Sort = "total" | "calls" | "errors" | "p99" | "name";

function Table({ rows, group }: { rows: SyscallRow[]; group: SyscallGroup }) {
  const selectable = group === "syscall";
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
    return rows.toSorted((a, b) => {
      const [x, y] = [key(a), key(b)];
      return typeof x === "string" ? x.localeCompare(String(y)) : Number(y) - Number(x);
    });
  }, [rows, sort]);
  const toggle = useCallback((key: string) => setOpen((was) => (was === key ? null : key)), []);
  const select = useCallback(
    (key: string) => setLook((prev) => withLook(prev, "sc", prev.sc === key ? undefined : key)),
    [setLook],
  );
  return (
    <table className="syscalls-table" data-testid="syscall-table">
      <thead>
        <tr>
          <SortHeader column="name" numeric={false} sort={sort} setSort={setSort} />
          <SortHeader column="calls" sort={sort} setSort={setSort} />
          <SortHeader column="errors" sort={sort} setSort={setSort} />
          <SortHeader column="total" sort={sort} setSort={setSort} />
          <th className="num">p50</th>
          <th className="num">p90</th>
          <SortHeader column="p99" sort={sort} setSort={setSort} />
          <th className="num">max</th>
          <th className="num">bytes</th>
          {/* oxlint-disable-next-line jsx-a11y/control-has-associated-label -- each row's button names what it shows */}
          <th />
        </tr>
      </thead>
      <tbody>
        {sorted.map((row) => (
          <SummaryRow
            key={row.key}
            row={row}
            group={group}
            selected={selectable && look.sc === row.key}
            open={open === row.key}
            toggle={toggle}
            select={selectable ? select : undefined}
          />
        ))}
      </tbody>
    </table>
  );
}

function SortHeader({
  column,
  numeric = true,
  sort,
  setSort,
}: {
  column: Sort;
  numeric?: boolean;
  sort: Sort;
  setSort: (sort: Sort) => void;
}) {
  const click = useCallback(() => setSort(column), [setSort, column]);
  return (
    <th
      className={numeric ? "num" : undefined}
      aria-sort={sort === column ? (column === "name" ? "ascending" : "descending") : undefined}
    >
      <button type="button" className="sort" onClick={click}>
        {column}
      </button>
    </th>
  );
}

const time = (value: number | null) => (value === null ? "–" : <Duration ns={value} />);

function SummaryRow({
  row,
  group,
  selected,
  open,
  toggle,
  select,
}: {
  row: SyscallRow;
  group: SyscallGroup;
  selected: boolean;
  open: boolean;
  toggle: (key: string) => void;
  select: ((key: string) => void) | undefined;
}) {
  const pick = useCallback(() => select?.(row.key), [select, row.key]);
  const pickName = useCallback(
    (event: MouseEvent) => {
      event.stopPropagation();
      select?.(row.key);
    },
    [select, row.key],
  );
  const disclose = useCallback(
    (event: MouseEvent) => {
      event.stopPropagation();
      toggle(row.key);
    },
    [toggle, row.key],
  );
  return (
    <>
      <tr
        className={selected ? "selected" : undefined}
        onClick={select ? pick : undefined}
        data-row={row.key}
      >
        <td className="mono name" title={row.name}>
          {row.errnos.length > 0 ? (
            <button
              type="button"
              className="disclose"
              aria-expanded={open}
              aria-label="Errors by errno"
              onClick={disclose}
            >
              {open ? "▾" : "▸"}
            </button>
          ) : null}
          {select ? (
            <button
              type="button"
              className="link-button"
              aria-pressed={selected}
              onClick={pickName}
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
        <td>
          <SamplesDuring during={formatDuringRow(group, row.key)} name={row.name} />
        </td>
      </tr>
      {open
        ? row.errnos.map((errno) => (
            <tr key={errno.name} className="errno-row">
              <td className="mono error-text">{errno.name}</td>
              <td className="num">{formatCount(errno.count)}</td>
              {/* oxlint-disable-next-line jsx-a11y/control-has-associated-label -- an empty cell that fills the row */}
              <td colSpan={8} />
            </tr>
          ))
        : null}
    </>
  );
}

export function SamplesDuring({
  during,
  name,
  children = "samples",
}: {
  during: string;
  name: string;
  children?: string;
}) {
  const { setLook, summary } = useProfile();

  const kernel = sampledKernel(summary.settings);
  const show = useCallback(
    (event: MouseEvent) => {
      event.stopPropagation();
      setLook((prev) => {
        const look = withLook(prev, "du", during);
        return kernel ? look : withLook(look, "sk", "1");
      }, true);
    },
    [setLook, during, kernel],
  );
  return (
    <button
      type="button"
      className="link-button samples-during"
      title={
        kernel
          ? "Keep only the samples taken during these calls"
          : "Keep only these calls' time, under the stacks that made them"
      }
      aria-label={`Samples during ${name}`}
      onClick={show}
    >
      {children}
    </button>
  );
}
