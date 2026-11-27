import { createEffect, createMemo, createSignal, For, Match, Show, Switch } from "solid-js";

import { assert } from "../../assert";
import { diffModeOf } from "../../profile/look";
import { type Changed, changeOf, formatPercent } from "../../profile/paint";
import { type CallTree, nameOf, revealed } from "../../profile/tree";
import { createVirtualizer, type SolidVirtualizer } from "../../virtual";
import { type Fetched, useProfile, useTree } from "./context";
import { NoSamples, ownTone, Share, Unanswered } from "./parts";
import { type ActiveRow, useActiveRow } from "./rows";
import { SearchRow, useFrameSearch, useTreeSearch } from "./Search";

const ROW = 20;

export function TreeView() {
  const focus = useProfile();
  const tree = useTree(() => focus().query);
  const [search, setSearch] = useFrameSearch();
  const treeSearch = useTreeSearch(() => tree().tree, search);
  return (
    <div class="pane grow">
      <div class="pane-head">{focus().query.invert ? "Callers" : "Call tree"}</div>
      <SearchRow search={search()} onSearch={setSearch} matched={treeSearch().note} reset={null} />
      <TreeRows
        tree={tree()}
        label="Call tree"
        reading={focus().query.invert ? "Reading callers…" : "Reading the call tree…"}
        matched={treeSearch().matched}
      />
    </div>
  );
}

/** Siblings arrive sorted by descending weight, so node + 1 is the hottest child; auto-expand down the spine until it drops below 50% of its parent. */
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

interface Opened {
  tree: CallTree | null;
  open: Set<number>;
  matched: Set<number> | null;
}

const NOTHING_OPEN: Opened = { tree: null, open: new Set(), matched: null };

export function TreeRows(props: {
  tree: Fetched;
  label: string;
  reading: string;
  matched?: Set<number> | null | undefined;
}) {
  const [body, setBody] = createSignal<HTMLDivElement | null>(null);
  const [opened, setOpened] = createSignal<Opened>(NOTHING_OPEN);

  const open = createMemo<Set<number>>(() => {
    const tree = props.tree.tree;
    const matched = props.matched ?? null;
    const current = opened();
    if (!tree) {
      return NOTHING_OPEN.open;
    }
    const base = current.tree === tree ? current.open : opening(tree);
    return matched && current.matched !== matched ? revealed(tree, base, matched) : base;
  });

  createEffect(
    () => ({ tree: props.tree.tree, matched: props.matched ?? null }),
    ({ tree, matched }) => {
      if (!tree) {
        return;
      }
      setOpened((prev) => {
        if (prev.tree === tree && prev.matched === matched) {
          return prev;
        }
        const kept = prev.tree === tree ? prev.open : opening(tree);
        return { tree, open: matched ? revealed(tree, kept, matched) : kept, matched };
      });
    },
  );

  const setOpen = (next: Set<number>) =>
    setOpened({ tree: props.tree.tree, open: next, matched: props.matched ?? null });

  const rows = createMemo(() => {
    const shown: number[] = [];
    const tree = props.tree.tree;
    if (!tree) {
      return shown;
    }
    const expanded = open();
    for (let node = 0; node < tree.count;) {
      shown.push(node);
      node = expanded.has(node) ? node + 1 : (tree.end[node] ?? node + 1);
    }
    return shown;
  });

  const virtual = createVirtualizer<HTMLDivElement, Element>(() => {
    const element = body();
    return {
      count: rows().length,
      getScrollElement: () => element,
      estimateSize: () => ROW,
      overscan: 30,
    };
  });
  const keys = useActiveRow(() => rows().length, virtual);

  return (
    <Switch>
      <Match when={props.tree.error || !props.tree.tree}>
        <Unanswered error={props.tree.error} reading={props.reading} />
      </Match>
      <Match when={props.tree.tree?.count === 0}>
        <NoSamples />
      </Match>
      <Match when={props.tree.tree}>
        {(tree) => (
          <TreeBody
            tree={tree()}
            rows={rows()}
            open={open()}
            setOpen={setOpen}
            bodyRef={setBody}
            virtual={virtual}
            keys={keys}
            label={props.label}
            current={props.tree.current}
            matched={props.matched ?? null}
          />
        )}
      </Match>
    </Switch>
  );
}

const TREE_COLUMNS = ["own", "total"];
const DIFF_TREE_COLUMNS = ["own", "total", "change"];

function TreeBody(props: {
  tree: CallTree;
  rows: number[];
  open: Set<number>;
  setOpen: (open: Set<number>) => void;
  bodyRef: (element: HTMLDivElement) => void;
  virtual: SolidVirtualizer<HTMLDivElement, Element>;
  keys: ActiveRow;
  label: string;
  current: boolean;
  matched: Set<number> | null;
}) {
  const focus = useProfile();
  useFirstMatch(
    () => props.rows,
    () => props.matched,
    props.keys,
  );
  const toggle = (node: number) => {
    const next = new Set(props.open);
    if (!next.delete(node)) {
      next.add(node);
    }
    props.setOpen(next);
  };
  const select = (node: number) => {
    const name = nameOf(props.tree, node);
    focus().setLook((prev) => ({ ...prev, fn: name }));
  };
  const choosePick = (index: number, node: number) => {
    props.keys.choose(index);
    select(node);
  };
  const chooseToggle = (index: number, node: number) => {
    props.keys.choose(index);
    toggle(node);
  };
  const parent = (node: number) => node + 1 < (props.tree.end[node] ?? 0);
  const onKey = (key: string) => {
    const active = props.keys.active();
    const node = props.rows[active];
    assert(node !== undefined, `active row ${active} is outside ${props.rows.length} rows`);
    if (key === "Enter") {
      select(node);
    } else if (key === "ArrowRight") {
      if (!parent(node)) {
        return false;
      }
      if (props.open.has(node)) {
        props.keys.move(active + 1);
      } else {
        toggle(node);
      }
    } else if (key === "ArrowLeft") {
      if (parent(node) && props.open.has(node)) {
        toggle(node);
      } else {
        const above = props.tree.parent[node] ?? -1;
        const caller = props.rows.indexOf(above);
        assert(caller >= 0 || above === -1, `node ${node} is shown but its caller ${above} is not`);
        if (caller < 0) {
          return false;
        }
        props.keys.move(caller);
      }
    } else {
      return false;
    }
    return true;
  };
  const keyDown = (event: KeyboardEvent) => props.keys.onKeyDown(event, onKey);

  return (
    <>
      <ColumnHead columns={props.tree.baseTotal === null ? TREE_COLUMNS : DIFF_TREE_COLUMNS} />
      <div
        ref={(node) => props.bodyRef(node)}
        class={`pane-body ${props.current ? "" : "stale"}`}
        role="tree"
        aria-label={props.label}
        tabindex={0}
        aria-activedescendant={
          props.rows.length > 0 ? props.keys.rowId(props.keys.active()) : undefined
        }
        onKeyDown={keyDown}
      >
        <div class="tree-rows" style={{ height: `${props.virtual.totalSize()}px` }}>
          <For
            each={props.virtual.items()}
            keyed={(item) => props.rows[item.index] ?? -1 - item.index}
          >
            {(item) => (
              <TreeRow
                id={props.keys.rowId(item().index)}
                tree={props.tree}
                node={props.rows[item().index] ?? 0}
                index={item().index}
                start={item().start}
                parent={parent(props.rows[item().index] ?? 0)}
                open={props.open.has(props.rows[item().index] ?? 0)}
                active={item().index === props.keys.active()}
                matched={props.matched?.has(props.rows[item().index] ?? 0) ?? false}
                pick={choosePick}
                twist={chooseToggle}
              />
            )}
          </For>
        </div>
      </div>
    </>
  );
}

const SHOWN = { visibility: "visible" } as const;
const HIDDEN = { visibility: "hidden" } as const;

function TreeRow(props: {
  id: string;
  tree: CallTree;
  node: number;
  index: number;
  start: number;
  parent: boolean;
  open: boolean;
  active: boolean;
  matched: boolean;
  pick: (index: number, node: number) => void;
  twist: (index: number, node: number) => void;
}) {
  const focus = useProfile();
  const depth = createMemo(() => props.tree.depth[props.node] ?? 0);
  const click = () => props.pick(props.index, props.node);
  const clickTwist = (event: MouseEvent) => {
    event.stopPropagation();
    props.twist(props.index, props.node);
  };
  const name = createMemo(() => nameOf(props.tree, props.node));
  const total = createMemo(() =>
    formatPercent(props.tree.total[props.node] ?? 0, props.tree.weight),
  );
  const own = createMemo(() => formatPercent(props.tree.own[props.node] ?? 0, props.tree.weight));
  const totalShare = createMemo(
    () => (props.tree.total[props.node] ?? 0) / Math.max(props.tree.weight, 1),
  );
  const ownShare = createMemo(
    () => (props.tree.own[props.node] ?? 0) / Math.max(props.tree.weight, 1),
  );
  const change = createMemo(() =>
    props.tree.baseTotal
      ? changeOf(
          diffModeOf(focus().look),
          [props.tree.total[props.node] ?? 0, props.tree.weight],
          [props.tree.baseTotal[props.node] ?? 0, props.tree.baseWeight ?? 0],
          focus().unit,
        )
      : null,
  );
  const ariaLabel = createMemo(() => {
    const current = change();
    return `${name()}, ${own()} own, ${total()} total${current ? `, ${current.text} change` : ""}`;
  });
  return (
    // oxlint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/interactive-supports-focus -- the tree keeps focus, names its active row, and takes the keys for its rows
    <div
      id={props.id}
      role="treeitem"
      aria-level={depth() + 1}
      aria-expanded={props.parent ? (props.open ? "true" : "false") : undefined}
      aria-selected={focus().look.fn === name() ? "true" : "false"}
      aria-label={ariaLabel()}
      class={`row tree-row ${focus().look.fn === name() ? "selected" : ""} ${props.active ? "active" : ""}`}
      style={{ transform: `translateY(${props.start}px)` }}
      onClick={click}
    >
      <Share share={ownShare()} tone={ownTone(ownShare(), "plain")}>
        {own()}
      </Share>
      <Share share={totalShare()} tone="dim">
        {total()}
      </Share>
      <Show when={change()}>{(current) => <Change change={current()} />}</Show>
      <span style={{ width: `${depth() * 12}px`, flex: "none" }} />
      <button
        type="button"
        class="twist"
        tabindex={-1}
        aria-hidden="true"
        style={props.parent ? SHOWN : HIDDEN}
        onClick={clickTwist}
      >
        {props.open ? "▾" : "▸"}
      </button>
      <span class={`name ${props.matched ? "matched" : ""}`} title={name()}>
        {name()}
      </span>
    </div>
  );
}

function useFirstMatch(rows: () => number[], matched: () => Set<number> | null, keys: ActiveRow) {
  let moved: Set<number> | null = null;
  createEffect(
    () => ({ rows: rows(), matched: matched() }),
    ({ rows: shown, matched: current }) => {
      if (!current || current === moved) {
        return;
      }
      moved = current;
      const first = shown.findIndex((node) => current.has(node));
      if (first >= 0) {
        keys.move(first);
      }
    },
  );
}

export const COLUMN_HELP: Record<string, string> = {
  own: "Time in the function itself",
  total: "Time in the function and what it calls",
  change: "Change from the base profile",
};

export function ColumnHead(props: { columns: string[]; end?: string | undefined }) {
  return (
    <div class="row column-head" aria-hidden="true">
      <For each={props.columns}>
        {(column) => (
          <span
            class={`percent ${column === "change" ? "change" : ""}`}
            title={COLUMN_HELP[column]}
          >
            <span class="hover-help">{column}</span>
          </span>
        )}
      </For>
      <span class="name">function</span>
      <Show when={props.end}>{(end) => <span class="end">{end()}</span>}</Show>
    </div>
  );
}

export function Change(props: { change: Changed }) {
  return (
    <span class={`percent change ${props.change.way}`} title="Change from the base">
      {props.change.text}
    </span>
  );
}
