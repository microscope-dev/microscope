import { useVirtualizer } from "@tanstack/react-virtual";
import { useMemo, useRef, useState } from "react";
import { diffModeOf } from "../../profile/look";
import { type Changed, changeOf, formatPercent } from "../../profile/paint";
import { type CallTree, decodeTree, nameOf } from "../../profile/tree";
import type { ProfileQuery, RequestError } from "../../protocol";
import { useAnswer, useProfile } from "./context";
import { Details } from "./Details";
import { Unanswered } from "./parts";
import { useActiveRow } from "./rows";

const ROW = 20;

/** The call tree of the query, with the selected function's details. */
export function TreeView() {
  const { query, look } = useProfile();
  const tree = useTree(query);
  return (
    <div className="pane grow">
      <div className="pane-head">{query.invert ? "Callers" : "Call tree"}</div>
      <TreeRows
        tree={tree}
        label="Call tree"
        reading={query.invert ? "Reading callers…" : "Reading the call tree…"}
      />
      {look.fn ? <Details name={look.fn} /> : null}
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
  const { look, setLook, unit } = useProfile();
  const tree = fetched.tree;
  const body = useRef<HTMLDivElement>(null);
  // Opened nodes belong to one tree; another answer opens its own.
  const [opened, setOpened] = useState<{ tree: CallTree | null; open: Set<number> }>({
    tree: null,
    open: new Set(),
  });
  const open = opened.tree === tree ? opened.open : tree ? opening(tree) : new Set<number>();
  const rows = useMemo(() => {
    const shown: number[] = [];
    if (!tree) {
      return shown;
    }
    for (let node = 0; node < tree.count; ) {
      shown.push(node);
      node = open.has(node) ? node + 1 : (tree.end[node] ?? node + 1);
    }
    return shown;
  }, [tree, open]);
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
  const toggle = (node: number) => {
    const next = new Set(open);
    if (!next.delete(node)) {
      next.add(node);
    }
    setOpened({ tree, open: next });
  };
  const select = (node: number) => {
    const name = nameOf(tree, node);
    setLook((look) => ({ ...look, fn: name }));
  };
  const parent = (node: number) => node + 1 < (tree.end[node] ?? 0);
  const onKey = (key: string) => {
    const node = rows[keys.active];
    if (node === undefined) {
      return false;
    }
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
        const caller = rows.indexOf(tree.parent[node] ?? -1);
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
  const diff = tree.baseTotal !== null;
  return (
    <>
      <ColumnHead columns={diff ? ["total", "own", "change"] : ["total", "own"]} />
      <div
        ref={body}
        className={`pane-body ${fetched.current ? "" : "stale"}`}
        role="tree"
        aria-label={label}
        tabIndex={0}
        aria-activedescendant={rows.length > 0 ? keys.rowId(keys.active) : undefined}
        onKeyDown={(event) => keys.onKeyDown(event, onKey)}
      >
        <div className="tree-rows" style={{ height: virtual.getTotalSize() }}>
          {virtual.getVirtualItems().map((item) => {
            const node = rows[item.index] ?? 0;
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
            const active = item.index === keys.active;
            return (
              // biome-ignore lint/a11y/useFocusableInteractive: the tree keeps focus and names its active row
              // biome-ignore lint/a11y/useKeyWithClickEvents: the tree takes the keys for its rows
              <div
                key={node}
                id={keys.rowId(item.index)}
                role="treeitem"
                aria-level={(tree.depth[node] ?? 0) + 1}
                aria-expanded={parent(node) ? open.has(node) : undefined}
                aria-selected={look.fn === name}
                aria-label={`${name}, ${total} total, ${own} own${change ? `, ${change.text} change` : ""}`}
                className={`row tree-row ${look.fn === name ? "selected" : ""} ${active ? "active" : ""}`}
                style={{ transform: `translateY(${item.start}px)` }}
                onClick={() => {
                  keys.choose(item.index);
                  select(node);
                }}
              >
                <span className="percent">{total}</span>
                <span className="percent dim">{own}</span>
                {change ? <Change change={change} /> : null}
                <span style={{ width: (tree.depth[node] ?? 0) * 12, flex: "none" }} />
                <button
                  type="button"
                  className="twist"
                  tabIndex={-1}
                  aria-hidden="true"
                  style={{ visibility: parent(node) ? "visible" : "hidden" }}
                  onClick={(event) => {
                    event.stopPropagation();
                    keys.choose(item.index);
                    toggle(node);
                  }}
                >
                  {open.has(node) ? "▾" : "▸"}
                </button>
                <span className="name" title={name}>
                  {name}
                </span>
              </div>
            );
          })}
        </div>
      </div>
    </>
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
