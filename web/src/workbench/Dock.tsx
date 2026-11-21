import { createMemo, createSignal, For, onSettled, Show } from "solid-js";

import { classOf } from "./classes";
import { DockContext, type DockSetup, useDock } from "./context";
import { pressWindow, rememberSize } from "./drag";
import { trackPointer } from "./gesture";
import { GroupView } from "./Group";
import { activate, dock, type GroupId, groups, type PaneId, type Rect, reframe } from "./layout";
import { keepOnScreen } from "./snap";
import { NodeView } from "./SplitView";
import { change, useLayout, workbench } from "./store";

import styles from "./Dock.module.css";

const css = classOf(styles);

export const NARROW = 720;

const LEAST = { width: 200, height: 120 } as const;

const narrowQuery = () => window.matchMedia(`(max-width: ${NARROW - 1}px)`);

export function useNarrow(): () => boolean {
  const [narrow, setNarrow] = createSignal(narrowQuery().matches);
  onSettled(() => {
    const query = narrowQuery();
    const changed = () => setNarrow(query.matches);
    setNarrow(query.matches);
    query.addEventListener("change", changed);
    return () => query.removeEventListener("change", changed);
  });
  return narrow;
}

export function Dock(props: { setup: DockSetup }) {
  const root = useLayout((layout) => layout.root);
  const maximized = useLayout((layout) => layout.maximized);
  const focused = useLayout((layout) => layout.focused);

  const stacked = useLayout((layout) => layout.floating.map((window) => window.group.id).join(" "));
  const windows = createMemo(() => {
    const value = stacked();
    return value === "" ? [] : (value.split(" ") as GroupId[]);
  });
  const sortedWindows = createMemo(() => windows().toSorted());
  const narrow = useNarrow();
  let element: HTMLDivElement | undefined;
  const size = useSize(() => element);

  return (
    <DockContext value={props.setup}>
      <div
        class={css("dock")}
        data-part="dock"
        ref={(el) => {
          element = el;
        }}
      >
        <Show
          when={narrow()}
          fallback={
            <>
              <div class={css("dock-tree")} data-part="tree">
                <Show when={maximized()} fallback={<NodeView node={root()} />}>
                  {(id) => <GroupView id={id()} />}
                </Show>
              </div>
              <Show when={size()}>
                {(bounds) => (
                  <For each={sortedWindows()}>
                    {(id) => (
                      <FloatingWindow id={id} bounds={bounds()} layer={windows().indexOf(id) + 1} />
                    )}
                  </For>
                )}
              </Show>
            </>
          }
        >
          <div class={[css("dock-tree"), css("dock-narrow")]} data-part="tree">
            <PanePicker />
            <GroupView id={focused()} />
          </div>
        </Show>
      </div>
    </DockContext>
  );
}

function useSize(element: () => HTMLDivElement | undefined) {
  const [size, setSize] = createSignal<{ width: number; height: number } | null>(null);
  onSettled(() => {
    const box = element();
    if (!box) {
      return;
    }
    const measure = () => setSize({ width: box.clientWidth, height: box.clientHeight });
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(box);
    return () => observer.disconnect();
  });
  return size;
}

function FloatingWindow(props: {
  id: GroupId;
  bounds: { width: number; height: number };
  layer: number;
}) {
  const found = useLayout((layout) =>
    layout.floating.find((window) => window.group.id === props.id),
  );
  return (
    <Show when={found()}>
      {(win) => (
        <WindowFrame id={props.id} placed={win().rect} bounds={props.bounds} layer={props.layer} />
      )}
    </Show>
  );
}

function WindowFrame(props: {
  id: GroupId;
  placed: Rect;
  bounds: { width: number; height: number };
  layer: number;
}) {
  const { kinds } = useDock();
  const title = useLayout((layout) => {
    const group = groups(layout).find((candidate) => candidate.id === props.id);
    const pane = group?.active ? layout.panes[group.active] : undefined;
    return pane ? kinds.get(pane.kind).title(pane) : "";
  });
  let element: HTMLDivElement | undefined;
  const rect = createMemo(() => keepOnScreen(props.placed, { x: 0, y: 0, ...props.bounds }));
  const style = createMemo(() => {
    const r = rect();
    return {
      left: `${r.x}px`,
      top: `${r.y}px`,
      width: `${r.width}px`,
      height: `${r.height}px`,
      "z-index": props.layer,
    };
  });

  const raise = () => {
    const { layout } = workbench.getState();
    if (layout.floating.at(-1)?.group.id !== props.id) {
      change((now) => reframe(now, props.id, props.placed));
    }
  };

  onSettled(() => {
    const box = element;
    if (!box) {
      return;
    }
    box.addEventListener("pointerdown", raise, { capture: true });
    return () => box.removeEventListener("pointerdown", raise, { capture: true });
  });

  const resizeKey = (event: KeyboardEvent) => {
    if (event.altKey || event.ctrlKey || event.metaKey) {
      return;
    }
    const step = event.shiftKey ? 64 : 16;
    const delta = {
      ArrowLeft: [-step, 0],
      ArrowRight: [step, 0],
      ArrowUp: [0, -step],
      ArrowDown: [0, step],
    }[event.key];
    if (delta) {
      event.preventDefault();
      change((layout) =>
        reframe(layout, props.id, grown(rect(), delta[0] ?? 0, delta[1] ?? 0, props.bounds)),
      );
    }
  };

  const gripDown = (event: PointerEvent & { currentTarget: HTMLButtonElement }) => {
    const box = element;
    if (!box) {
      return;
    }
    const startRect = rect();
    const bounds = props.bounds;
    const sized = (at: PointerEvent) =>
      grown(startRect, at.clientX - event.clientX, at.clientY - event.clientY, bounds);
    const resize = (to: Rect) => {
      box.style.width = `${to.width}px`;
      box.style.height = `${to.height}px`;
    };
    const tracked = trackPointer(event, {
      move: (moved) => resize(sized(moved)),
      up: (released) => change((layout) => reframe(layout, props.id, sized(released))),
      cancel: () => resize(startRect),
    });
    if (tracked) {
      event.preventDefault();
      event.currentTarget.focus();
    }
  };

  const press = (event: PointerEvent & { currentTarget: HTMLDivElement }) => {
    const target = event.target as Element;
    if (
      !target.closest("button") &&
      (target.closest("[data-part=window-bar]") ||
        target.matches(`[data-group="${props.id}"] > [role="tablist"]`))
    ) {
      pressWindow(event, props.id, event.currentTarget, rect());
    }
  };

  const dockRight = () => {
    rememberSize(workbench.getState().layout, props.id);
    change((layout) => dock(layout, props.id, { type: "edge", side: "right" }));
  };

  return (
    <div
      ref={(el) => {
        element = el;
      }}
      class={css("dock-window")}
      data-part="window"
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a <dialog> is hidden until opened and brings its own focus handling; this window is always shown
      role="dialog"
      aria-label={title()}
      style={style()}
      onPointerDown={press}
    >
      <div class={css("window-bar")} data-part="window-bar">
        <span class={css("window-title")}>{title()}</span>
        <button
          type="button"
          class={css("window-dock")}
          data-part="window-dock"
          title="Dock this window on the right"
          aria-label={`Dock ${title()}`}
          onClick={dockRight}
        >
          Dock
        </button>
      </div>
      <GroupView id={props.id} />
      <button
        type="button"
        class={css("window-grip")}
        data-part="window-grip"
        data-keys="escape"
        aria-label={`Resize ${title()}`}
        title="Resize with arrow keys; hold Shift for larger steps"
        onKeyDown={resizeKey}
        onPointerDown={gripDown}
      />
    </div>
  );
}

function grown(rect: Rect, dx: number, dy: number, bounds: { width: number; height: number }) {
  return {
    ...rect,
    width: Math.min(bounds.width - rect.x, Math.max(LEAST.width, rect.width + dx)),
    height: Math.min(bounds.height - rect.y, Math.max(LEAST.height, rect.height + dy)),
  };
}

function PanePicker() {
  const { kinds } = useDock();
  const options = useLayout((layout) =>
    groups(layout)
      .flatMap((group) => group.tabs)
      .map((pane) => {
        const found = layout.panes[pane];
        return `${pane}\u0000${found ? kinds.get(found.kind).title(found) : pane}`;
      })
      .join("\u0001"),
  );
  const items = createMemo(() => {
    const raw = options();
    return raw === ""
      ? []
      : raw.split("\u0001").map((entry) => entry.split("\u0000") as [string, string]);
  });
  const shown = useLayout(
    (layout) => groups(layout).find((group) => group.id === layout.focused)?.active ?? "",
  );
  const choose = (event: Event & { currentTarget: HTMLSelectElement }) => {
    const pane = event.currentTarget.value as PaneId;
    change((layout) => activate(layout, pane));
    const found = workbench.getState().layout.panes[pane];
    if (found) {
      kinds.get(found.kind).chosen?.(found);
    }
  };
  return (
    <select class={css("pane-picker")} aria-label="Pane" value={shown()} onChange={choose}>
      <For each={items()} keyed={([pane]) => pane}>
        {(entry) => <option value={entry()[0]}>{entry()[1]}</option>}
      </For>
    </select>
  );
}
