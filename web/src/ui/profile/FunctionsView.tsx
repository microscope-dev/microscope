import type { JSX } from "@solidjs/web";
import { createMemo, createSignal, For, Match, Show, Switch } from "solid-js";

import { assert } from "../../assert";
import { diffModeOf } from "../../profile/look";
import { changeOf, formatCount, formatPercent, formatWeight } from "../../profile/paint";
import type { ProfileBaseWeight, ProfileCost, ProfileKey, ProfileQuery } from "../../protocol";
import { createVirtualizer } from "../../virtual";
import { trackPointer } from "../../workbench/gesture";
import { LEAST } from "../../workbench/SplitView";
import { Code, SourceLines } from "./Annotated";
import { useAnswer, useProfile, useTree } from "./context";
import { LookButton, NoSamples, ownTone, Share, Unanswered } from "./parts";
import { useActiveRow } from "./rows";
import { SearchRow, useFrameSearch } from "./Search";
import { Change, ColumnHead, TreeRows } from "./TreeView";

const ROW = 20;

type Order = "own" | "total";

export function FunctionsView() {
  const focus = useProfile();
  const answer = useAnswer("profileFunctions", () => focus().query);
  const [order, setOrder] = createSignal<Order>("own");

  const [split, setSplit] = createSignal(0.5);
  const splitStyle = createMemo<JSX.CSSProperties>(() => ({
    "grid-template-rows": rowsOf(split()),
  }));
  const [search, setSearch] = useFrameSearch();
  const [body, setBody] = createSignal<HTMLDivElement | null>(null);
  const mode = createMemo(() => diffModeOf(focus().look));
  const everyRow = createMemo(() => answer().data?.rows);
  const rows = createMemo(() => {
    const data = answer().data;
    const allRows = everyRow() ?? [];
    const needle = search().toLowerCase();
    const all = needle
      ? allRows.filter((row) => data?.keys[row.key]?.name.toLowerCase().includes(needle))
      : allRows;
    if (order() === "total") {
      return all.toSorted((a, b) => b.total - a.total || b.own - a.own);
    }

    return data?.base && mode() === "time"
      ? all.toSorted((a, b) => Math.abs(b.own - b.baseOwn) - Math.abs(a.own - a.baseOwn))
      : all;
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
  const weight = createMemo(() => answer().data?.weight ?? 0);
  const names = createMemo(() => answer().data?.keys);
  const base = createMemo(() => answer().data?.base ?? null);
  const select = (index: number) => {
    const list = rows();
    const row = list[index];
    assert(row !== undefined, `row ${index} is outside ${list.length} rows`);
    const name = names()?.[row.key]?.name;
    if (name !== undefined) {
      focus().setLook((prev) => ({ ...prev, fn: name }));
    }
  };
  const pick = (index: number) => {
    keys.choose(index);
    select(index);
  };
  const keyDown = (event: KeyboardEvent) =>
    keys.onKeyDown(event, (key) => {
      if (key !== "Enter") {
        return false;
      }
      select(keys.active());
      return true;
    });

  const matchedNote = createMemo(() => {
    const all = everyRow();
    return search() && all
      ? `${formatCount(rows().length)} of ${formatCount(all.length, "function")} match`
      : null;
  });

  return (
    <div class="profile-functions" style={focus().look.fn ? splitStyle() : undefined}>
      <div class="pane grow">
        <div class="pane-head">
          Functions
          <span class="count">{formatCount(rows().length)}</span>
          <div class="segmented">
            <For each={ORDERS}>
              {(each) => (
                <OrderButton order={each} pressed={order() === each} setOrder={setOrder}>
                  by {each === "own" && base() ? "change" : each}
                </OrderButton>
              )}
            </For>
          </div>
        </div>
        <SearchRow search={search()} onSearch={setSearch} matched={matchedNote()} reset={null} />
        <ColumnHead columns={base() ? DIFF_COLUMNS : COLUMNS} end="module" />
        <Switch>
          <Match when={answer().error || !answer().data}>
            <Unanswered error={answer().error} reading="Reading functions…" />
          </Match>
          <Match when={everyRow()?.length === 0}>
            <NoSamples />
          </Match>
          <Match when={rows().length === 0}>
            <div class="empty">No function matches.</div>
          </Match>
          <Match when={answer().data}>
            <div
              ref={(node) => setBody(node)}
              class={`pane-body ${answer().current ? "" : "stale"}`}
              // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a virtual list a select cannot hold
              role="listbox"
              aria-label="Functions"
              tabindex={0}
              aria-activedescendant={keys.rowId(keys.active())}
              onKeyDown={keyDown}
            >
              <div class="tree-rows" style={{ height: `${virtual.totalSize()}px` }}>
                <For
                  each={virtual.items()}
                  keyed={(item) => rows()[item.index]?.key ?? -1 - item.index}
                >
                  {(item) => {
                    const entry = createMemo(() => {
                      const row = rows()[item().index];
                      const key = row ? answer().data?.keys[row.key] : undefined;
                      return row && key ? { row, key } : null;
                    });
                    return (
                      <Show when={entry()}>
                        {(shown) => (
                          <FunctionRow
                            id={keys.rowId(item().index)}
                            row={shown().row}
                            fn={shown().key}
                            index={item().index}
                            start={item().start}
                            active={item().index === keys.active()}
                            weight={weight()}
                            base={base()}
                            pick={pick}
                          />
                        )}
                      </Show>
                    );
                  }}
                </For>
              </div>
            </div>
          </Match>
        </Switch>
      </div>
      <Show when={focus().look.fn}>
        {(fn) => <Selected name={fn()} split={split()} setSplit={setSplit} />}
      </Show>
    </div>
  );
}

const ORDERS = ["own", "total"] as const;
const COLUMNS = ["own", "total"];
const DIFF_COLUMNS = ["own", "total", "change"];

function OrderButton(props: {
  order: Order;
  pressed: boolean;
  setOrder: (order: Order) => void;
  children: JSX.Element;
}) {
  return (
    <button
      type="button"
      aria-pressed={props.pressed ? "true" : "false"}
      onClick={() => props.setOrder(props.order)}
    >
      {props.children}
    </button>
  );
}

function FunctionRow(props: {
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
  const focus = useProfile();
  const selected = createMemo(() => focus().look.fn === props.fn.name);
  const own = createMemo(() => formatPercent(props.row.own, props.weight));
  const total = createMemo(() => formatPercent(props.row.total, props.weight));
  const ownShare = createMemo(() => props.row.own / Math.max(props.weight, 1));
  const change = createMemo(() =>
    props.base
      ? changeOf(
          diffModeOf(focus().look),
          [props.row.own, props.weight],
          [props.row.baseOwn, props.base.weight],
          focus().unit,
        )
      : null,
  );
  const ariaLabel = createMemo(() => {
    const current = change();
    return `${props.fn.name}${props.fn.module ? ` in ${props.fn.module}` : ""}, ${own()} own, ${total()} total${current ? `, ${current.text} change` : ""}`;
  });
  return (
    // oxlint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/interactive-supports-focus -- the list keeps focus, names its active row, and takes the keys for its rows
    <div
      id={props.id}
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a row of a virtual list, not a select's option
      role="option"
      aria-selected={selected() ? "true" : "false"}
      aria-label={ariaLabel()}
      class={`row tree-row ${selected() ? "selected" : ""} ${props.active ? "active" : ""}`}
      style={{ transform: `translateY(${props.start}px)` }}
      title={formatWeight(props.row.total, focus().unit)}
      onClick={() => props.pick(props.index)}
    >
      <Share share={ownShare()} tone={ownTone(ownShare(), "plain")}>
        {own()}
      </Share>
      <Share share={props.row.total / Math.max(props.weight, 1)} tone="dim">
        {total()}
      </Share>
      <Show when={change()}>{(current) => <Change change={current()} />}</Show>
      <span class="name">{props.fn.name}</span>
      <Show when={props.fn.module}>{(module) => <span class="end">{module()}</span>}</Show>
    </div>
  );
}

const PANELS = [
  [undefined, "Callers and callees"],
  ["lines", "Source"],
  ["code", "Source and assembly"],
] as const;

function Selected(props: { name: string; split: number; setSplit: (split: number) => void }) {
  const focus = useProfile();
  return (
    <div class="profile-selected">
      <Divider split={props.split} setSplit={props.setSplit} />
      <div class="pane-head">
        <div class="segmented">
          <For each={PANELS}>
            {([ann, label]) => (
              <LookButton field="ann" value={ann} pressed={focus().look.ann === ann}>
                {label}
              </LookButton>
            )}
          </For>
        </div>
        <Show when={focus().look.ann && focus().query.transforms.length > 0}>
          <span class="muted">Every selected sample counts here, without the transforms.</span>
        </Show>
      </div>
      <Switch fallback={<Sandwich query={focus().query} name={props.name} />}>
        <Match when={focus().look.ann === "lines"}>
          <SourceLines name={props.name} />
        </Match>
        <Match when={focus().look.ann === "code"}>
          <Code name={props.name} />
        </Match>
      </Switch>
    </div>
  );
}

const STEP = 16;

function Divider(props: { split: number; setSplit: (split: number) => void }) {
  const down = (event: PointerEvent & { currentTarget: HTMLDivElement }) => {
    const box = event.currentTarget.closest<HTMLElement>(".profile-functions");
    if (!box) {
      return;
    }
    const startSplit = props.split;
    const show = (at: number) => {
      box.style.gridTemplateRows = rowsOf(at);
    };
    const tracked = trackPointer(event, {
      move: (moved) => show(splitAt(box, moved.clientY)),
      up: (released) => props.setSplit(splitAt(box, released.clientY)),
      cancel: () => show(startSplit),
    });
    if (tracked) {
      event.preventDefault();
      event.currentTarget.focus();
    }
  };
  const key = (event: KeyboardEvent & { currentTarget: HTMLDivElement }) => {
    const step = event.shiftKey ? STEP * 4 : STEP;
    const by = event.key === "ArrowUp" ? -step : event.key === "ArrowDown" ? step : 0;
    const box = event.currentTarget.closest<HTMLElement>(".profile-functions");
    if (by === 0 || !box) {
      return;
    }
    event.preventDefault();
    const { top, height } = box.getBoundingClientRect();
    props.setSplit(splitAt(box, top + props.split * height + by));
  };
  const even = () => props.setSplit(0.5);
  return (
    <div
      class="profile-divider"
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a focusable separator that resizes is a widget, which <hr> is not
      role="separator"
      aria-orientation="horizontal"
      aria-label="Resize the functions and the selected function"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(props.split * 100)}
      tabindex={0}
      data-keys="escape"
      onPointerDown={down}
      onKeyDown={key}
      onDblClick={even}
    />
  );
}

function splitAt(box: HTMLElement, y: number): number {
  const { top, height } = box.getBoundingClientRect();
  const least = Math.min(LEAST, height / 2);
  return Math.max(least, Math.min(height - least, y - top)) / Math.max(height, 1);
}

function rowsOf(split: number): string {
  return `minmax(0, ${split * 100}fr) minmax(0, ${(1 - split) * 100}fr)`;
}

const CALLERS_HELP = "Functions that call this one.";
const CALLEES_HELP = "Functions this one calls.";

function Sandwich(props: { query: ProfileQuery; name: string }) {
  const focused = createMemo(() => ({
    ...props.query,
    transforms: [...props.query.transforms, { op: "focus" as const, name: props.name }],
  }));
  const callers = useTree(() => ({ ...focused(), invert: !props.query.invert }));
  const callees = useTree(focused);
  return (
    <div class="profile-sandwich" data-testid="sandwich">
      <div class="pane grow">
        <div class="pane-head">
          <span class="hover-help" title={CALLERS_HELP}>
            Callers of
          </span>{" "}
          <span class="subject">{props.name}</span>
        </div>
        <TreeRows
          tree={callers()}
          label="Callers"
          reading={`Reading the callers of ${props.name}…`}
        />
      </div>
      <div class="pane grow">
        <div class="pane-head">
          <span class="hover-help" title={CALLEES_HELP}>
            Callees of
          </span>{" "}
          <span class="subject">{props.name}</span>
        </div>
        <TreeRows
          tree={callees()}
          label="Callees"
          reading={`Reading the callees of ${props.name}…`}
        />
      </div>
    </div>
  );
}
