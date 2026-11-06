import { useVirtualizer, type Virtualizer } from "@tanstack/react-virtual";
import {
  type KeyboardEvent,
  type MouseEvent,
  type RefObject,
  useCallback,
  useMemo,
  useRef,
  useState,
} from "react";

import { assert } from "../../assert";
import { diffModeOf } from "../../profile/look";
import { type Changed, changeOf, formatPercent } from "../../profile/paint";
import { type CallTree, decodeTree, nameOf } from "../../profile/tree";
import type { ProfileQuery, RequestError } from "../../protocol";
import { useAnswer, useProfile } from "./context";
import { Details } from "./Details";
import { Unanswered } from "./parts";
import { type ActiveRow, useActiveRow } from "./rows";

const ROW = 20;

/** The call tree of the query, with the selected function's details. */
export function TreeView() {
  const { query, look } = useProfile();
  const tree = useTree(query);
  return (
    <div className="pane grow">
      <div className="pane-head">{query.invert ? "Callers" : "Call tree"}</div>
      {look.fn ? (
        <Details name={look.fn} />
      ) : (
        <div className="profile-details muted">Click a row to select it.</div>
      )}
      <TreeRows
        tree={tree}
        label="Call tree"
        reading={query.invert ? "Reading callers…" : "Reading the call tree…"}
      />
    </div>
  );
}

export interface Fetched {
  tree: CallTree | null;
  error: RequestError | undefined;
  current: boolean;
}

export function useTree(query: ProfileQuery): Fetched {
  const answer = useAnswer("profileTree", query);
  const tree = useMemo(
    () => (answer.data ? decodeTree(answer.data, answer.data.payload) : null),
    [answer.data],
  );
  return { tree, error: answer.error, current: answer.current };
}

/** The nodes a tree opens with: down the heaviest path while each child
 * holds at least half its caller's weight. */
function opening(tree: CallTree): Set<number> {
  const open = new Set<number>();
  for (let root = 0; root < tree.count; root = tree.end[root] ?? tree.count) {
    let node = root;
    while (node + 1 < (tree.end[node] ?? 0)) {
      open.add(node);
      const child = node + 1;
      if ((tree.total[child] ?? 0) * 2 < (tree.total[node] ?? 0)) {
        break;
      }
      node = child;
    }
  }
  return open;
}

/** A tree's rows, each node under its caller, opened and closed by its
 * twist or by Left and Right; choosing a row, or Enter, selects its
 * function. */
export function TreeRows({
  tree: fetched,
  label,
  reading,
}: {
  tree: Fetched;
  label: string;
  reading: string;
}) {
  const tree = fetched.tree;
  const body = useRef<HTMLDivElement>(null);
  // Opened nodes belong to one tree; another answer opens its own.
  const [opened, setOpened] = useState<{ tree: CallTree | null; open: Set<number> }>({
    tree: null,
    open: new Set(),
  });
  const open = useMemo(
    () => (opened.tree === tree ? opened.open : tree ? opening(tree) : new Set<number>()),
    [opened, tree],
  );
  const rows = useMemo(() => {
    const shown: number[] = [];
    if (!tree) {
      return shown;
    }
    for (let node = 0; node < tree.count;) {
      shown.push(node);
      node = open.has(node) ? node + 1 : (tree.end[node] ?? node + 1);
    }
    return shown;
  }, [tree, open]);
  // oxlint-disable-next-line react/incompatible-library -- the app is not built with the React Compiler
  const virtual = useVirtualizer({
    count: rows.length,
    getScrollElement: () => body.current,
    estimateSize: () => ROW,
    overscan: 30,
  });
  const keys = useActiveRow(rows.length, virtual);
  if (fetched.error || !tree) {
    return <Unanswered error={fetched.error} reading={reading} />;
  }
  if (tree.count === 0) {
    return <div className="empty">No samples are selected.</div>;
  }
  return (
    <TreeBody
      tree={tree}
      rows={rows}
      open={open}
      setOpen={setOpened}
      body={body}
      virtual={virtual}
      keys={keys}
      label={label}
      current={fetched.current}
    />
  );
}

const TREE_COLUMNS = ["total", "own"];
const DIFF_TREE_COLUMNS = ["total", "own", "change"];

function TreeBody({
  tree,
  rows,
  open,
  setOpen,
  body,
  virtual,
  keys,
  label,
  current,
}: {
  tree: CallTree;
  rows: number[];
  open: Set<number>;
  setOpen: (opened: { tree: CallTree; open: Set<number> }) => void;
  body: RefObject<HTMLDivElement | null>;
  virtual: Virtualizer<HTMLDivElement, Element>;
  keys: ActiveRow;
  label: string;
  current: boolean;
}) {
  const { setLook } = useProfile();
  const toggle = useCallback(
    (node: number) => {
      const next = new Set(open);
      if (!next.delete(node)) {
        next.add(node);
      }
      setOpen({ tree, open: next });
    },
    [tree, open, setOpen],
  );
  const select = useCallback(
    (node: number) => {
      const name = nameOf(tree, node);
      setLook((prev) => ({ ...prev, fn: name }));
    },
    [tree, setLook],
  );
  const { choose } = keys;
  const choosePick = useCallback(
    (index: number, node: number) => {
      choose(index);
      select(node);
    },
    [choose, select],
  );
  const chooseToggle = useCallback(
    (index: number, node: number) => {
      choose(index);
      toggle(node);
    },
    [choose, toggle],
  );
  const parent = (node: number) => node + 1 < (tree.end[node] ?? 0);
  const onKey = (key: string) => {
    const node = rows[keys.active];
    assert(node !== undefined, `active row ${keys.active} is outside ${rows.length} rows`);
    if (key === "Enter") {
      select(node);
    } else if (key === "ArrowRight") {
      if (!parent(node)) {
        return false;
      }
      if (open.has(node)) {
        keys.move(keys.active + 1);
      } else {
        toggle(node);
      }
    } else if (key === "ArrowLeft") {
      if (parent(node) && open.has(node)) {
        toggle(node);
      } else {
        const above = tree.parent[node] ?? -1;
        const caller = rows.indexOf(above);
        assert(caller >= 0 || above === -1, `node ${node} is shown but its caller ${above} is not`);
        if (caller < 0) {
          return false;
        }
        keys.move(caller);
      }
    } else {
      return false;
    }
    return true;
  };
  // oxlint-disable-next-line react-perf/jsx-no-new-function-as-prop -- it reads the active row, which changes with every key
  const keyDown = (event: KeyboardEvent) => keys.onKeyDown(event, onKey);
  const size = virtual.getTotalSize();
  const sized = useMemo(() => ({ height: size }), [size]);
  return (
    <>
      <ColumnHead columns={tree.baseTotal === null ? TREE_COLUMNS : DIFF_TREE_COLUMNS} />
      <div
        ref={body}
        className={`pane-body ${current ? "" : "stale"}`}
        role="tree"
        aria-label={label}
        tabIndex={0}
        aria-activedescendant={rows.length > 0 ? keys.rowId(keys.active) : undefined}
        onKeyDown={keyDown}
      >
        <div className="tree-rows" style={sized}>
          {virtual.getVirtualItems().map((item) => {
            const node = rows[item.index] ?? 0;
            return (
              <TreeRow
                key={node}
                id={keys.rowId(item.index)}
                tree={tree}
                node={node}
                index={item.index}
                start={item.start}
                parent={parent(node)}
                open={open.has(node)}
                active={item.index === keys.active}
                pick={choosePick}
                twist={chooseToggle}
              />
            );
          })}
        </div>
      </div>
    </>
  );
}

const SHOWN = { visibility: "visible" } as const;
const HIDDEN = { visibility: "hidden" } as const;

function TreeRow({
  id,
  tree,
  node,
  index,
  start,
  parent,
  open,
  active,
  pick,
  twist,
}: {
  id: string;
  tree: CallTree;
  node: number;
  index: number;
  start: number;
  parent: boolean;
  open: boolean;
  active: boolean;
  pick: (index: number, node: number) => void;
  twist: (index: number, node: number) => void;
}) {
  const { look, unit } = useProfile();
  const depth = tree.depth[node] ?? 0;
  const placed = useMemo(() => ({ transform: `translateY(${start}px)` }), [start]);
  const indent = useMemo(() => ({ width: depth * 12, flex: "none" }), [depth]);
  const click = useCallback(() => pick(index, node), [pick, index, node]);
  const clickTwist = useCallback(
    (event: MouseEvent) => {
      event.stopPropagation();
      twist(index, node);
    },
    [twist, index, node],
  );
  const name = nameOf(tree, node);
  const total = formatPercent(tree.total[node] ?? 0, tree.weight);
  const own = formatPercent(tree.own[node] ?? 0, tree.weight);
  const change = tree.baseTotal
    ? changeOf(
        diffModeOf(look),
        [tree.total[node] ?? 0, tree.weight],
        [tree.baseTotal[node] ?? 0, tree.baseWeight ?? 0],
        unit,
      )
    : null;
  return (
    // oxlint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/interactive-supports-focus -- the tree keeps focus, names its active row, and takes the keys for its rows
    <div
      id={id}
      role="treeitem"
      aria-level={depth + 1}
      aria-expanded={parent ? open : undefined}
      aria-selected={look.fn === name}
      aria-label={`${name}, ${total} total, ${own} own${change ? `, ${change.text} change` : ""}`}
      className={`row tree-row ${look.fn === name ? "selected" : ""} ${active ? "active" : ""}`}
      style={placed}
      onClick={click}
    >
      <span className="percent">{total}</span>
      <span className="percent dim">{own}</span>
      {change ? <Change change={change} /> : null}
      <span style={indent} />
      <button
        type="button"
        className="twist"
        tabIndex={-1}
        aria-hidden="true"
        style={parent ? SHOWN : HIDDEN}
        onClick={clickTwist}
      >
        {open ? "▾" : "▸"}
      </button>
      <span className="name" title={name}>
        {name}
      </span>
    </div>
  );
}

/** What each column of the rows below holds, over them. Each row names
 * its own columns for screen readers. */
export function ColumnHead({ columns, end }: { columns: string[]; end?: string }) {
  return (
    <div className="row column-head" aria-hidden="true">
      {columns.map((column) => (
        <span key={column} className={`percent ${column === "change" ? "change" : ""}`}>
          {column}
        </span>
      ))}
      <span className="name">function</span>
      {end ? <span className="end">{end}</span> : null}
    </div>
  );
}

/** A change from a diff's base, colored as the flame graph is. */
export function Change({ change }: { change: Changed }) {
  return (
    <span className={`percent change ${change.way}`} title="Change from the base">
      {change.text}
    </span>
  );
}
