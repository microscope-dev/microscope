// The dock's splits: children side by side or stacked, sharing the space by
// weight, with a separator between each two that resizes them by pointer or
// by keyboard. A drag moves only the two children's styles until it ends, so
// it renders nothing until the layout takes the new weights.

import { Fragment, type KeyboardEvent, memo, type PointerEvent, useRef } from "react";
import { assert } from "../assert";
import { useDock } from "./context";
import { GroupView } from "./Group";
import { type Layout, type Node, resize, type Split, type SplitId, splits } from "./layout";
import { snapSplitter } from "./snap";
import { change, useLayout } from "./store";

/** The least a pane gets along a split, in pixels. */
export const LEAST = 120;

/** How far a separator moves for one arrow key press, in pixels. */
const STEP = 16;

export const NodeView = memo(function NodeView({ node }: { node: Node }) {
  return node.type === "group" ? <GroupView id={node.id} /> : <SplitView id={node.id} />;
});

function splitIn(layout: Layout, id: SplitId): Split | null {
  return splits(layout.root).find((split) => split.id === id) ?? null;
}

const SplitView = memo(function SplitView({ id }: { id: SplitId }) {
  const split = useLayout((layout) => splitIn(layout, id));
  const box = useRef<HTMLDivElement>(null);
  if (!split) {
    return null;
  }
  return (
    <div className={`split split-${split.axis}`} ref={box} data-split={id}>
      {split.children.map((child, index) => (
        <Fragment key={child.node.id}>
          {index > 0 && <Separator split={split} index={index} box={box} />}
          <div className="split-child" style={{ flexGrow: child.weight }}>
            <NodeView node={child.node} />
          </div>
        </Fragment>
      ))}
    </div>
  );
});

/** What a separator's drag measures once, as it starts. */
interface Measure {
  /** The split's start and length along its axis. */
  start: number;
  length: number;
  /** Where the two children the separator divides begin and end. */
  from: number;
  to: number;
  /** Separators of other splits along the same axis, to line up with. */
  neighbors: number[];
  before: HTMLElement;
  after: HTMLElement;
}

function Separator({
  split,
  index,
  box,
}: {
  split: Split;
  index: number;
  box: React.RefObject<HTMLDivElement | null>;
}) {
  const { kinds } = useDock();
  const row = split.axis === "row";
  const first = split.children[index - 1];
  const second = split.children[index];
  assert(first && second, `separator ${index} of ${split.children.length} children`);
  const shared = first.weight + second.weight;
  const drag = useRef<Measure | null>(null);
  const panes = useLayout((layout) => layout.panes);
  const name = (node: Node): string => {
    const group = node.type === "group" ? node : leftmost(node);
    const pane = group.active ? panes[group.active] : undefined;
    return pane ? kinds.get(pane.kind).title(pane) : "panes";
  };

  const measure = (): Measure => {
    const element = box.current;
    assert(element, "a separator outside its split");
    const children = [...element.children].filter((child) =>
      child.classList.contains("split-child"),
    ) as HTMLElement[];
    const before = children[index - 1];
    const after = children[index];
    assert(before && after, `separator ${index} has no children beside it`);
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
  };

  /** The two children's weights with the separator at `at`. */
  const shares = (measured: Measure, at: number): [number, number] => {
    const span = measured.to - measured.from;
    const least = Math.min(LEAST, span / 2);
    const clamped = Math.max(measured.from + least, Math.min(measured.to - least, at));
    const part = (clamped - measured.from) / span;
    return [shared * part, shared * (1 - part)];
  };

  const commit = ([a, b]: [number, number]) => {
    const weights = split.children.map((child, at) =>
      at === index - 1 ? a : at === index ? b : child.weight,
    );
    change((layout) => resize(layout, split.id, weights));
  };

  const settle = (measured: Measure, at: number, free: boolean) => {
    const span = measured.to - measured.from;
    const least = Math.min(LEAST, span / 2);
    return snapSplitter({
      start: measured.start,
      length: measured.length,
      at,
      least: measured.from + least,
      most: measured.to - least,
      neighbors: measured.neighbors,
      free,
    });
  };

  const down = (event: PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) {
      return;
    }
    event.preventDefault();
    // Focused, so Escape reaches it to cancel the drag.
    event.currentTarget.focus();
    event.currentTarget.setPointerCapture(event.pointerId);
    drag.current = measure();
  };
  const moved = (event: PointerEvent<HTMLDivElement>) => {
    const measured = drag.current;
    if (!measured) {
      return;
    }
    const [a, b] = shares(
      measured,
      settle(measured, row ? event.clientX : event.clientY, event.altKey),
    );
    measured.before.style.flexGrow = String(a);
    measured.after.style.flexGrow = String(b);
  };
  const up = (event: PointerEvent<HTMLDivElement>) => {
    const measured = drag.current;
    drag.current = null;
    if (!measured) {
      return;
    }
    commit(shares(measured, settle(measured, row ? event.clientX : event.clientY, event.altKey)));
  };
  const cancel = () => {
    const measured = drag.current;
    drag.current = null;
    if (measured) {
      measured.before.style.flexGrow = String(first.weight);
      measured.after.style.flexGrow = String(second.weight);
    }
  };

  const key = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) {
      return;
    }
    if (event.key === "Escape" && drag.current) {
      cancel();
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      change((layout) =>
        resize(
          layout,
          split.id,
          split.children.map(() => 1),
        ),
      );
      return;
    }
    const step = event.shiftKey ? STEP * 4 : STEP;
    const moves: Record<string, (measured: Measure, at: number) => number> = row
      ? { ArrowLeft: (_, at) => at - step, ArrowRight: (_, at) => at + step }
      : { ArrowUp: (_, at) => at - step, ArrowDown: (_, at) => at + step };
    moves.Home = (measured) => measured.from;
    moves.End = (measured) => measured.to;
    const by = moves[event.key];
    if (!by) {
      return;
    }
    event.preventDefault();
    const measured = measure();
    const at = measured.from + (measured.to - measured.from) * (first.weight / shared);
    commit(shares(measured, by(measured, at)));
  };

  return (
    // biome-ignore lint/a11y/useSemanticElements: a focusable separator that resizes is a widget, which <hr> is not
    <div
      className="separator-bar"
      role="separator"
      aria-orientation={row ? "vertical" : "horizontal"}
      aria-label={`Resize ${name(first.node)} and ${name(second.node)}`}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round((100 * first.weight) / shared)}
      tabIndex={0}
      data-keys="escape"
      onPointerDown={down}
      onPointerMove={moved}
      onPointerUp={up}
      onPointerCancel={cancel}
      onLostPointerCapture={() => drag.current && cancel()}
      onDoubleClick={() =>
        change((layout) =>
          resize(
            layout,
            split.id,
            split.children.map(() => 1),
          ),
        )
      }
      onKeyDown={key}
    />
  );
}

function leftmost(node: Node): Node & { type: "group" } {
  return node.type === "group" ? node : leftmost((node.children[0] as { node: Node }).node);
}
