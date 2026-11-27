import { createMemo, For, Show } from "solid-js";

import { assert } from "../assert";
import { classOf } from "./classes";
import { useDock } from "./context";
import { trackPointer } from "./gesture";
import { GroupView } from "./Group";
import {
  type Child,
  type Layout,
  type Node,
  resize,
  type Split,
  type SplitId,
  splits,
} from "./layout";
import { snapSplitter } from "./snap";
import { change, useLayout } from "./store";

import styles from "./SplitView.module.css";

const css = classOf(styles);

export const LEAST = 120;

const STEP = 16;

export function NodeView(props: { node: Node }) {
  return (
    <Show when={props.node.type === "group"} fallback={<SplitView id={(props.node as Split).id} />}>
      <GroupView id={(props.node as Node & { type: "group" }).id} />
    </Show>
  );
}

function splitIn(layout: Layout, id: SplitId): Split | null {
  return splits(layout.root).find((split) => split.id === id) ?? null;
}

function SplitView(props: { id: SplitId }) {
  const split = useLayout((layout) => splitIn(layout, props.id));
  const hidden = useLayout((layout) => layout.hidden.join(" "));
  let box: HTMLDivElement | undefined;

  const shown = createMemo(() => {
    const current = split();
    if (!current) {
      return [] as number[];
    }
    const hiddenIds = hidden().split(" ");
    return current.children.flatMap((child, index) =>
      hiddenIds.includes(child.node.id) ? [] : [index],
    );
  });

  // Rescale visible children to fill the space of hidden siblings without mutating stored weights.
  const scale = createMemo(() => {
    const current = split();
    if (!current) {
      return 1;
    }
    const total = shown().reduce(
      (sum, index) => sum + (current.children[index] as Child).weight,
      0,
    );
    return total > 0 ? 1 / total : 1;
  });

  const entries = createMemo(() => {
    const current = split();
    if (!current) {
      return [];
    }
    const visible = shown();
    return visible.map((index, at) => {
      const child = current.children[index] as Child;
      return {
        id: child.node.id,
        index,
        at,
        previous: visible[at - 1],
        child,
      };
    });
  });

  return (
    <Show when={split()}>
      {(currentSplit) => (
        <div
          class={[css("split"), css(`split-${currentSplit().axis}`)]}
          ref={(el) => {
            box = el;
          }}
          data-split={props.id}
        >
          <For each={entries()} keyed={(entry) => entry.id}>
            {(entry) => (
              <>
                <Show when={entry().previous !== undefined}>
                  <Separator
                    split={currentSplit()}
                    before={entry().previous as number}
                    after={entry().index}
                    at={entry().at}
                    scale={scale()}
                    box={() => box}
                  />
                </Show>
                <SplitChild node={entry().child.node} grow={entry().child.weight * scale()} />
              </>
            )}
          </For>
        </div>
      )}
    </Show>
  );
}

function SplitChild(props: { node: Node; grow: number }) {
  return (
    <div class={css("split-child")} data-part="split-child" style={{ "flex-grow": props.grow }}>
      <NodeView node={props.node} />
    </div>
  );
}

interface Measure {
  start: number;
  length: number;

  from: number;
  to: number;

  neighbors: number[];
  before: HTMLElement;
  after: HTMLElement;
}

function Separator(props: {
  split: Split;
  before: number;
  after: number;
  at: number;

  scale: number;
  box: () => HTMLDivElement | undefined;
}) {
  const { kinds } = useDock();
  const row = createMemo(() => props.split.axis === "row");
  const pair = createMemo(() => {
    const first = props.split.children[props.before];
    const second = props.split.children[props.after];
    assert(
      first && second,
      `separator ${props.before}-${props.after} of ${props.split.children.length}`,
    );
    return { first, second, shared: first.weight + second.weight };
  });
  const panes = useLayout((layout) => layout.panes);
  const name = (node: Node): string => {
    const group = node.type === "group" ? node : leftmost(node);
    const pane = group.active ? panes()[group.active] : undefined;
    return pane ? kinds.get(pane.kind).title(pane) : "panes";
  };

  const down = (event: PointerEvent & { currentTarget: HTMLDivElement }) => {
    const isRow = row();
    const { first, second, shared } = pair();
    const scale = props.scale;
    const split = props.split;
    const beforeIndex = props.before;
    const afterIndex = props.after;
    const measured = measure(props.box(), props.at, isRow);
    const place = (pointer: PointerEvent) =>
      shares(
        measured,
        shared,
        settle(measured, isRow ? pointer.clientX : pointer.clientY, pointer.altKey),
      );
    // Mutate flexGrow on the DOM during drag to avoid reactive store and localStorage writes per frame.
    const show = ([a, b]: readonly [number, number]) => {
      measured.before.style.flexGrow = String(a * scale);
      measured.after.style.flexGrow = String(b * scale);
    };
    const tracked = trackPointer(event, {
      move: (moved) => show(place(moved)),
      up: (released) => commit(split, beforeIndex, afterIndex, place(released)),
      cancel: () => show([first.weight, second.weight]),
    });
    if (tracked) {
      event.preventDefault();
      event.currentTarget.focus();
    }
  };
  const even = () => evenOut(props.split);

  const key = (event: KeyboardEvent) => {
    if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) {
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      evenOut(props.split);
      return;
    }
    const isRow = row();
    const step = event.shiftKey ? STEP * 4 : STEP;
    const moves: Record<string, (measured: Measure, position: number) => number> = isRow
      ? {
          ArrowLeft: (_, position) => position - step,
          ArrowRight: (_, position) => position + step,
        }
      : {
          ArrowUp: (_, position) => position - step,
          ArrowDown: (_, position) => position + step,
        };
    moves.Home = (measured) => measured.from;
    moves.End = (measured) => measured.to;
    const by = moves[event.key];
    if (!by) {
      return;
    }
    event.preventDefault();
    const { first, shared } = pair();
    const measured = measure(props.box(), props.at, isRow);
    const position = measured.from + (measured.to - measured.from) * (first.weight / shared);
    commit(
      props.split,
      props.before,
      props.after,
      shares(measured, shared, by(measured, position)),
    );
  };

  return (
    <div
      class={css("separator-bar")}
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a focusable separator that resizes is a widget, which <hr> is not
      role="separator"
      aria-orientation={row() ? "vertical" : "horizontal"}
      aria-label={`Resize ${name(pair().first.node)} and ${name(pair().second.node)}`}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round((100 * pair().first.weight) / pair().shared)}
      tabindex={0}
      data-keys="escape"
      onPointerDown={down}
      onDblClick={even}
      onKeyDown={key}
    />
  );
}

function measure(element: HTMLDivElement | undefined, at: number, row: boolean): Measure {
  assert(element, "a separator outside its split");
  const children = [...element.children].filter(
    (child) => (child as HTMLElement).dataset.part === "split-child",
  ) as HTMLElement[];
  const before = children[at - 1];
  const after = children[at];
  assert(before && after, `separator ${at} has no children beside it`);
  const along = (rect: DOMRect) =>
    row ? { start: rect.left, end: rect.right } : { start: rect.top, end: rect.bottom };
  const whole = along(element.getBoundingClientRect());
  const orientation = row ? "vertical" : "horizontal";
  const neighbors = [
    ...document.querySelectorAll<HTMLElement>(
      `[role="separator"][aria-orientation="${orientation}"]`,
    ),
  ]
    .filter((other) => other.parentElement !== element)
    .map((other) => {
      const rect = other.getBoundingClientRect();
      return row ? rect.left + rect.width / 2 : rect.top + rect.height / 2;
    });
  return {
    start: whole.start,
    length: whole.end - whole.start,
    from: along(before.getBoundingClientRect()).start,
    to: along(after.getBoundingClientRect()).end,
    neighbors,
    before,
    after,
  };
}

function shares(measured: Measure, shared: number, position: number): [number, number] {
  const span = measured.to - measured.from;
  const least = Math.min(LEAST, span / 2);
  const clamped = Math.max(measured.from + least, Math.min(measured.to - least, position));
  const part = (clamped - measured.from) / span;
  return [shared * part, shared * (1 - part)];
}

function settle(measured: Measure, position: number, free: boolean): number {
  const span = measured.to - measured.from;
  const least = Math.min(LEAST, span / 2);
  return snapSplitter({
    start: measured.start,
    length: measured.length,
    at: position,
    least: measured.from + least,
    most: measured.to - least,
    neighbors: measured.neighbors,
    free,
  });
}

function commit(split: Split, beforeIndex: number, afterIndex: number, [a, b]: [number, number]) {
  const weights = split.children.map((child, index) =>
    index === beforeIndex ? a : index === afterIndex ? b : child.weight,
  );
  change((layout) => resize(layout, split.id, weights));
}

function evenOut(split: Split) {
  change((layout) =>
    resize(
      layout,
      split.id,
      split.children.map(() => 1),
    ),
  );
}

function leftmost(node: Node): Node & { type: "group" } {
  return node.type === "group" ? node : leftmost((node.children[0] as { node: Node }).node);
}
