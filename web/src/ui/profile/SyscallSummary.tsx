import { createMemo, createSignal, For, Match, Show, Switch } from "solid-js";

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

export function Summary(props: { summary: Requested<SyscallSummary>; query: SyscallQuery }) {
  const focus = useProfile();
  const group = createMemo<SyscallGroup>(() => focus().look.sg ?? "syscall");
  const regroup = (event: Event & { currentTarget: HTMLSelectElement }) => {
    const chosen = event.currentTarget.value as SyscallGroup;
    focus().setLook((prev) =>
      withLook(
        // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
        withLook(prev, "sc", undefined),
        "sg",
        chosen === "syscall" ? undefined : chosen,
      ),
    );
  };
  return (
    <div class="syscalls-summary">
      <div class="syscalls-split">
        <div class="pane syscalls-table-pane">
          <div class="pane-head">
            <select class="input profile-by" aria-label="Group" value={group()} onChange={regroup}>
              <For each={GROUPS}>{(each) => <option value={each.group}>{each.label}</option>}</For>
            </select>
          </div>
          <Switch>
            <Match when={props.summary.data}>
              {(data) => <Table rows={data().rows} group={group()} />}
            </Match>
            <Match when={!props.summary.error}>
              <p class="muted pad">Reading the summary…</p>
            </Match>
          </Switch>
        </div>
        <Show when={group() === "syscall" ? focus().look.sc : undefined}>
          {(call) => <Detail call={call()} query={props.query} />}
        </Show>
      </div>
    </div>
  );
}

type Sort = "total" | "calls" | "errors" | "p99" | "name";

function Table(props: { rows: SyscallRow[]; group: SyscallGroup }) {
  const selectable = createMemo(() => props.group === "syscall");
  const focus = useProfile();
  const [sort, setSort] = createSignal<Sort>("total");
  const [open, setOpen] = createSignal<string | null>(null);
  const sorted = createMemo(() => {
    const by = sort();
    const key = (row: SyscallRow): number | string =>
      by === "name"
        ? row.name
        : by === "calls"
          ? row.calls
          : by === "errors"
            ? row.errors
            : by === "p99"
              ? (row.p99Ns ?? 0)
              : row.totalNs;
    return props.rows.toSorted((a, b) => {
      const [x, y] = [key(a), key(b)];
      return typeof x === "string" ? x.localeCompare(String(y)) : Number(y) - Number(x);
    });
  });
  const toggle = (key: string) => setOpen((was) => (was === key ? null : key));
  const select = (key: string) =>
    focus().setLook((prev) => withLook(prev, "sc", prev.sc === key ? undefined : key));
  return (
    <table class="syscalls-table" data-testid="syscall-table">
      <thead>
        <tr>
          <SortHeader column="name" numeric={false} sort={sort()} setSort={setSort} />
          <SortHeader column="calls" sort={sort()} setSort={setSort} />
          <SortHeader column="errors" sort={sort()} setSort={setSort} />
          <SortHeader column="total" sort={sort()} setSort={setSort} />
          <th class="num">p50</th>
          <th class="num">p90</th>
          <SortHeader column="p99" sort={sort()} setSort={setSort} />
          <th class="num">max</th>
          <th class="num">bytes</th>
          {/* oxlint-disable-next-line jsx-a11y/control-has-associated-label -- each row's button names what it shows */}
          <th />
        </tr>
      </thead>
      <tbody>
        <For each={sorted()} keyed={(row) => row.key}>
          {(row) => (
            <SummaryRow
              row={row()}
              group={props.group}
              selected={selectable() && focus().look.sc === row().key}
              open={open() === row().key}
              toggle={toggle}
              select={selectable() ? select : undefined}
            />
          )}
        </For>
      </tbody>
    </table>
  );
}

function SortHeader(props: {
  column: Sort;
  numeric?: boolean | undefined;
  sort: Sort;
  setSort: (sort: Sort) => void;
}) {
  return (
    <th
      class={props.numeric === false ? undefined : "num"}
      aria-sort={
        props.sort === props.column
          ? props.column === "name"
            ? "ascending"
            : "descending"
          : undefined
      }
    >
      <button type="button" class="sort" onClick={() => props.setSort(props.column)}>
        {props.column}
      </button>
    </th>
  );
}

function TimeCell(props: { value: number | null }) {
  return (
    <Show when={props.value === null ? null : { ns: props.value }} fallback="–">
      {(shown) => <Duration ns={shown().ns} />}
    </Show>
  );
}

function SummaryRow(props: {
  row: SyscallRow;
  group: SyscallGroup;
  selected: boolean;
  open: boolean;
  toggle: (key: string) => void;
  select: ((key: string) => void) | undefined;
}) {
  const pick = () => props.select?.(props.row.key);
  const pickName = (event: MouseEvent) => {
    event.stopPropagation();
    props.select?.(props.row.key);
  };
  const disclose = (event: MouseEvent) => {
    event.stopPropagation();
    props.toggle(props.row.key);
  };
  return (
    <>
      <tr class={props.selected ? "selected" : undefined} onClick={pick} data-row={props.row.key}>
        <td class="mono name" title={props.row.name}>
          <Show when={props.row.errnos.length > 0}>
            <button
              type="button"
              class="disclose"
              aria-expanded={props.open ? "true" : "false"}
              aria-label="Errors by errno"
              onClick={disclose}
            >
              {props.open ? "▾" : "▸"}
            </button>
          </Show>
          <Show when={props.select} fallback={props.row.name}>
            <button
              type="button"
              class="link-button"
              aria-pressed={props.selected ? "true" : "false"}
              onClick={pickName}
            >
              {props.row.name}
            </button>
          </Show>
        </td>
        <td class="num">{formatCount(props.row.calls)}</td>
        <td class={`num ${props.row.errors > 0 ? "error-text" : ""}`}>
          {formatCount(props.row.errors)}
        </td>
        <td class="num">
          <Duration ns={props.row.totalNs} />
        </td>
        <td class="num">
          <TimeCell value={props.row.p50Ns} />
        </td>
        <td class="num">
          <TimeCell value={props.row.p90Ns} />
        </td>
        <td class="num">
          <TimeCell value={props.row.p99Ns} />
        </td>
        <td class="num">
          <Show when={props.row.timed > 0} fallback="–">
            <Duration ns={props.row.maxNs} />
          </Show>
        </td>
        <td class="num">
          <Show when={props.row.bytes > 0}>
            <Bytes bytes={props.row.bytes} />
          </Show>
        </td>
        <td>
          <SamplesDuring
            during={formatDuringRow(props.group, props.row.key)}
            name={props.row.name}
          />
        </td>
      </tr>
      <Show when={props.open}>
        <For each={props.row.errnos} keyed={(errno) => errno.name}>
          {(errno) => (
            <tr class="errno-row">
              <td class="mono error-text">{errno().name}</td>
              <td class="num">{formatCount(errno().count)}</td>
              {/* oxlint-disable-next-line jsx-a11y/control-has-associated-label -- an empty cell that fills the row */}
              <td colspan={8} />
            </tr>
          )}
        </For>
      </Show>
    </>
  );
}

export function SamplesDuring(props: {
  during: string;
  name: string;
  children?: string | undefined;
}) {
  const focus = useProfile();

  const kernel = createMemo(() => sampledKernel(focus().summary.settings));
  const show = (event: MouseEvent) => {
    event.stopPropagation();
    const isKernel = kernel();
    focus().setLook((prev) => {
      const look = withLook(prev, "du", props.during);
      return isKernel ? look : withLook(look, "sk", "1");
    }, true);
  };
  return (
    <button
      type="button"
      class="link-button samples-during"
      title={
        kernel()
          ? "Keep only the samples taken during these calls"
          : "Keep only these calls' time, under the stacks that made them"
      }
      aria-label={`Samples during ${props.name}`}
      onClick={show}
    >
      {props.children ?? "samples"}
    </button>
  );
}
