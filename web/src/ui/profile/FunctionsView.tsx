import { useVirtualizer } from "@tanstack/react-virtual";
import { type KeyboardEvent, type ReactNode, useCallback, useMemo, useRef, useState } from "react";

import { assert } from "../../assert";
import { diffModeOf } from "../../profile/look";
import { changeOf, formatCount, formatPercent, formatWeight } from "../../profile/paint";
import type { ProfileBaseWeight, ProfileCost, ProfileKey, ProfileQuery } from "../../protocol";
import { Code, SourceLines } from "./Annotated";
import { useAnswer, useProfile, useTree } from "./context";
import { LookButton, ownTone, Share, Unanswered } from "./parts";
import { useActiveRow } from "./rows";
import { SearchRow, useFrameSearch } from "./Search";
import { Change, ColumnHead, TreeRows } from "./TreeView";

const ROW = 20;

type Order = "own" | "total";

/** Every function's cost, or those a search matches, and for the selected
 * one a sandwich: its callers above and its callees below, merged from
 * every call. */
export function FunctionsView() {
  const { query, look, setLook } = useProfile();
  const answer = useAnswer("profileFunctions", query);
  const [order, setOrder] = useState<Order>("own");
  const [search, setSearch] = useFrameSearch();
  const body = useRef<HTMLDivElement>(null);
  const mode = diffModeOf(look);
  const everyRow = answer.data?.rows;
  const rows = useMemo(() => {
    const needle = search.toLowerCase();
    const all = needle
      ? (everyRow ?? []).filter((row) =>
          answer.data?.keys[row.key]?.name.toLowerCase().includes(needle),
        )
      : (everyRow ?? []);
    if (order === "total") {
      return all.toSorted((a, b) => b.total - a.total || b.own - a.own);
    }
    // The server orders a diff by change in share.
    return answer.data?.base && mode === "time"
      ? all.toSorted((a, b) => Math.abs(b.own - b.baseOwn) - Math.abs(a.own - a.baseOwn))
      : all;
  }, [answer.data, everyRow, search, order, mode]);
  // oxlint-disable-next-line react/incompatible-library -- the app is not built with the React Compiler
  const virtual = useVirtualizer({
    count: rows.length,
    getScrollElement: () => body.current,
    estimateSize: () => ROW,
    overscan: 30,
  });
  const keys = useActiveRow(rows.length, virtual);
  const weight = answer.data?.weight ?? 0;
  const names = answer.data?.keys;
  const base = answer.data?.base ?? null;
  const select = useCallback(
    (index: number) => {
      const row = rows[index];
      assert(row !== undefined, `row ${index} is outside ${rows.length} rows`);
      const name = names?.[row.key]?.name;
      if (name !== undefined) {
        setLook((prev) => ({ ...prev, fn: name }));
      }
    },
    [rows, names, setLook],
  );
  const { choose } = keys;
  const pick = useCallback(
    (index: number) => {
      choose(index);
      select(index);
    },
    [choose, select],
  );
  // oxlint-disable-next-line react-perf/jsx-no-new-function-as-prop -- it reads the active row, which changes with every key
  const keyDown = (event: KeyboardEvent) =>
    keys.onKeyDown(event, (key) => {
      if (key !== "Enter") {
        return false;
      }
      select(keys.active);
      return true;
    });
  const size = virtual.getTotalSize();
  const sized = useMemo(() => ({ height: size }), [size]);
  return (
    <div className="profile-functions">
      <div className="pane grow">
        <div className="pane-head">
          Functions
          <span className="count">{formatCount(rows.length)}</span>
          <div className="segmented">
            {ORDERS.map((each) => (
              <OrderButton key={each} order={each} pressed={order === each} setOrder={setOrder}>
                by {each === "own" && base ? "change" : each}
              </OrderButton>
            ))}
          </div>
        </div>
        <SearchRow
          search={search}
          onSearch={setSearch}
          matched={
            search && everyRow
              ? `${formatCount(rows.length)} of ${formatCount(everyRow.length, "function")} match`
              : null
          }
          reset={null}
        />
        <ColumnHead columns={base ? DIFF_COLUMNS : COLUMNS} end="module" />
        {answer.error || !answer.data ? (
          <Unanswered error={answer.error} reading="Reading functions…" />
        ) : everyRow?.length === 0 ? (
          <div className="empty">No samples are selected.</div>
        ) : rows.length === 0 ? (
          <div className="empty">No function matches.</div>
        ) : (
          <div
            ref={body}
            className={`pane-body ${answer.current ? "" : "stale"}`}
            // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a virtual list a select cannot hold
            role="listbox"
            aria-label="Functions"
            tabIndex={0}
            aria-activedescendant={keys.rowId(keys.active)}
            onKeyDown={keyDown}
          >
            <div className="tree-rows" style={sized}>
              {virtual.getVirtualItems().map((item) => {
                const row = rows[item.index];
                const key = row ? answer.data?.keys[row.key] : undefined;
                if (!row || !key) {
                  return null;
                }
                return (
                  <FunctionRow
                    // Functions of one name in two modules are two rows.
                    key={row.key}
                    id={keys.rowId(item.index)}
                    row={row}
                    fn={key}
                    index={item.index}
                    start={item.start}
                    active={item.index === keys.active}
                    weight={weight}
                    base={base}
                    pick={pick}
                  />
                );
              })}
            </div>
          </div>
        )}
      </div>
      {look.fn ? <Selected name={look.fn} /> : null}
    </div>
  );
}

const ORDERS = ["own", "total"] as const;
const COLUMNS = ["own", "total"];
const DIFF_COLUMNS = ["own", "total", "change"];

function OrderButton({
  order,
  pressed,
  setOrder,
  children,
}: {
  order: Order;
  pressed: boolean;
  setOrder: (order: Order) => void;
  children: ReactNode;
}) {
  const click = useCallback(() => setOrder(order), [setOrder, order]);
  return (
    <button type="button" aria-pressed={pressed} onClick={click}>
      {children}
    </button>
  );
}

function FunctionRow({
  id,
  row,
  fn,
  index,
  start,
  active,
  weight,
  base,
  pick,
}: {
  id: string;
  row: ProfileCost;
  fn: ProfileKey;
  index: number;
  start: number;
  active: boolean;
  weight: number;
  base: ProfileBaseWeight | null;
  pick: (index: number) => void;
}) {
  const { look, unit } = useProfile();
  const placed = useMemo(() => ({ transform: `translateY(${start}px)` }), [start]);
  const click = useCallback(() => pick(index), [pick, index]);
  const selected = look.fn === fn.name;
  const own = formatPercent(row.own, weight);
  const total = formatPercent(row.total, weight);
  const ownShare = row.own / Math.max(weight, 1);
  const change = base
    ? changeOf(diffModeOf(look), [row.own, weight], [row.baseOwn, base.weight], unit)
    : null;
  return (
    // oxlint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/interactive-supports-focus -- the list keeps focus, names its active row, and takes the keys for its rows
    <div
      id={id}
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a row of a virtual list, not a select's option
      role="option"
      aria-selected={selected}
      aria-label={`${fn.name}${fn.module ? ` in ${fn.module}` : ""}, ${own} own, ${total} total${change ? `, ${change.text} change` : ""}`}
      className={`row tree-row ${selected ? "selected" : ""} ${active ? "active" : ""}`}
      style={placed}
      title={formatWeight(row.total, unit)}
      onClick={click}
    >
      <Share share={ownShare} tone={ownTone(ownShare, "plain")}>
        {own}
      </Share>
      <Share share={row.total / Math.max(weight, 1)} tone="dim">
        {total}
      </Share>
      {change ? <Change change={change} /> : null}
      <span className="name">{fn.name}</span>
      {fn.module ? <span className="end">{fn.module}</span> : null}
    </div>
  );
}

const PANELS = [
  [undefined, "Callers and callees"],
  ["lines", "Source"],
  ["code", "Assembly"],
] as const;

/** The selected function: its callers and callees, or its samples on its
 * source lines or on its instructions. */
function Selected({ name }: { name: string }) {
  const { query, look } = useProfile();
  return (
    <div className="profile-selected">
      <div className="pane-head">
        <div className="segmented">
          {PANELS.map(([ann, label]) => (
            <LookButton key={label} field="ann" value={ann} pressed={look.ann === ann}>
              {label}
            </LookButton>
          ))}
        </div>
        {look.ann && query.transforms.length > 0 ? (
          <span className="muted">Every selected sample counts here, without the transforms.</span>
        ) : null}
      </div>
      {look.ann === "lines" ? (
        <SourceLines name={name} />
      ) : look.ann === "code" ? (
        <Code name={name} />
      ) : (
        <Sandwich query={query} name={name} />
      )}
    </div>
  );
}

const CALLERS_HELP = "Functions that call this one.";
const CALLEES_HELP = "Functions this one calls.";

/** A function's callers, inverted from it, and its callees, from it. */
function Sandwich({ query, name }: { query: ProfileQuery; name: string }) {
  const focused = useMemo(
    () => ({ ...query, transforms: [...query.transforms, { op: "focus" as const, name }] }),
    [query, name],
  );
  const callers = useTree(useMemo(() => ({ ...focused, invert: !query.invert }), [focused, query]));
  const callees = useTree(focused);
  return (
    <div className="profile-sandwich" data-testid="sandwich">
      <div className="pane grow">
        <div className="pane-head">
          <span className="hover-help" title={CALLERS_HELP}>
            Callers of
          </span>{" "}
          <span className="subject">{name}</span>
        </div>
        <TreeRows tree={callers} label="Callers" reading={`Reading the callers of ${name}…`} />
      </div>
      <div className="pane grow">
        <div className="pane-head">
          <span className="hover-help" title={CALLEES_HELP}>
            Callees of
          </span>{" "}
          <span className="subject">{name}</span>
        </div>
        <TreeRows tree={callees} label="Callees" reading={`Reading the callees of ${name}…`} />
      </div>
    </div>
  );
}
