// The dock: the layout's tree of splits and groups, filling the space it is
// given, or the one maximized group until it is restored, with floating
// windows above. In a narrow window it shows one group at a time, with a
// list to choose any pane from, and nothing floats.

import {
  type ChangeEvent,
  type KeyboardEvent,
  memo,
  type PointerEvent,
  type RefObject,
  useCallback,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";

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

/** Below this width the dock shows one group at a time. */
export const NARROW = 720;

/** The least a floating window is resized to. */
const LEAST = { width: 200, height: 120 } as const;

const narrowQuery = () => window.matchMedia(`(max-width: ${NARROW - 1}px)`);

export function useNarrow(): boolean {
  return useSyncExternalStore(
    (changed) => {
      const query = narrowQuery();
      query.addEventListener("change", changed);
      return () => query.removeEventListener("change", changed);
    },
    () => narrowQuery().matches,
  );
}

export function Dock({ setup }: { setup: DockSetup }) {
  const root = useLayout((layout) => layout.root);
  const maximized = useLayout((layout) => layout.maximized);
  const focused = useLayout((layout) => layout.focused);
  // Bottom to top, as text so an unchanged list is the same value.
  const stacked = useLayout((layout) => layout.floating.map((window) => window.group.id).join(" "));
  const windows = stacked === "" ? [] : (stacked.split(" ") as GroupId[]);
  const narrow = useNarrow();
  const element = useRef<HTMLDivElement>(null);
  const size = useSize(element);

  return (
    <DockContext value={setup}>
      <div className={css("dock")} data-part="dock" ref={element}>
        {narrow ? (
          <div className={`${css("dock-tree")} ${css("dock-narrow")}`} data-part="tree">
            <PanePicker />
            <GroupView id={focused} />
          </div>
        ) : (
          <>
            <div className={css("dock-tree")} data-part="tree">
              {maximized ? <GroupView id={maximized} /> : <NodeView node={root} />}
            </div>
            {size &&
              // In a fixed order, raised by z-index: moving a window's element
              // would drop the pointer it is being dragged by.
              windows
                .toSorted()
                .map((id) => (
                  <FloatingWindow key={id} id={id} bounds={size} layer={windows.indexOf(id) + 1} />
                ))}
          </>
        )}
      </div>
    </DockContext>
  );
}

/** The dock's size as it changes, measured before the first paint so
 * windows a kept layout floats show at once. */
function useSize(element: RefObject<HTMLDivElement | null>) {
  const [size, setSize] = useState<{ width: number; height: number } | null>(null);
  useLayoutEffect(() => {
    const box = element.current;
    if (!box) {
      return;
    }
    const measure = () => setSize({ width: box.clientWidth, height: box.clientHeight });
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(box);
    return () => observer.disconnect();
  }, [element]);
  return size;
}

/** A group floating above the dock: a title bar that moves it and docks it
 * when dropped on a tab strip or the dock's edge, and a grip that resizes
 * it. It is drawn inside the dock, however the dock has shrunk. */
const FloatingWindow = memo(function FloatingWindow({
  id,
  bounds,
  layer,
}: {
  id: GroupId;
  bounds: { width: number; height: number };
  layer: number;
}) {
  const found = useLayout((layout) => layout.floating.find((window) => window.group.id === id));
  return found ? <WindowFrame id={id} placed={found.rect} bounds={bounds} layer={layer} /> : null;
});

function WindowFrame({
  id,
  placed,
  bounds,
  layer,
}: {
  id: GroupId;
  /** Where the layout puts the window, which may be off the dock now. */
  placed: Rect;
  bounds: { width: number; height: number };
  layer: number;
}) {
  const { kinds } = useDock();
  const title = useLayout((layout) => {
    const group = groups(layout).find((candidate) => candidate.id === id);
    const pane = group?.active ? layout.panes[group.active] : undefined;
    return pane ? kinds.get(pane.kind).title(pane) : "";
  });
  const element = useRef<HTMLDivElement>(null);
  const rect = useMemo(() => keepOnScreen(placed, { x: 0, y: 0, ...bounds }), [placed, bounds]);
  const style = useMemo(
    () => ({ left: rect.x, top: rect.y, width: rect.width, height: rect.height, zIndex: layer }),
    [rect, layer],
  );

  const resizeKey = useCallback(
    (event: KeyboardEvent<HTMLButtonElement>) => {
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
        change((layout) => reframe(layout, id, grown(rect, delta[0] ?? 0, delta[1] ?? 0, bounds)));
      }
    },
    [id, rect, bounds],
  );

  // The window is resized by hand as the pointer moves, and in the layout
  // once, when it lets go.
  const gripDown = useCallback(
    (event: PointerEvent<HTMLButtonElement>) => {
      const box = element.current;
      if (!box) {
        return;
      }
      const sized = (at: globalThis.PointerEvent) =>
        grown(rect, at.clientX - event.clientX, at.clientY - event.clientY, bounds);
      const resize = (to: Rect) => {
        box.style.width = `${to.width}px`;
        box.style.height = `${to.height}px`;
      };
      const tracked = trackPointer(event, {
        move: (moved) => resize(sized(moved)),
        up: (released) => change((layout) => reframe(layout, id, sized(released))),
        cancel: () => resize(rect),
      });
      if (tracked) {
        event.preventDefault();
        // So the arrow keys go on resizing it.
        event.currentTarget.focus();
      }
    },
    [id, rect, bounds],
  );

  // Working in a window raises it.
  const raise = useCallback(() => {
    const { layout } = workbench.getState();
    if (layout.floating.at(-1)?.group.id !== id) {
      change((now) => reframe(now, id, placed));
    }
  }, [id, placed]);
  const press = useCallback(
    (event: PointerEvent<HTMLDivElement>) => {
      const target = event.target as Element;
      if (
        !target.closest("button") &&
        (target.closest("[data-part=window-bar]") ||
          target.matches(`[data-group="${id}"] > [role="tablist"]`))
      ) {
        pressWindow(event, id, event.currentTarget, rect);
      }
    },
    [id, rect],
  );
  const dockRight = useCallback(() => {
    rememberSize(workbench.getState().layout, id);
    change((layout) => dock(layout, id, { type: "edge", side: "right" }));
  }, [id]);

  return (
    <div
      ref={element}
      className={css("dock-window")}
      data-part="window"
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a <dialog> is hidden until opened and brings its own focus handling; this window is always shown
      role="dialog"
      aria-label={title}
      style={style}
      onPointerDownCapture={raise}
      onPointerDown={press}
    >
      <div className={css("window-bar")} data-part="window-bar">
        <span className={css("window-title")}>{title}</span>
        <button
          type="button"
          className={css("window-dock")}
          data-part="window-dock"
          title="Dock this window on the right"
          aria-label={`Dock ${title}`}
          onClick={dockRight}
        >
          Dock
        </button>
      </div>
      <GroupView id={id} />
      <button
        type="button"
        className={css("window-grip")}
        data-part="window-grip"
        data-keys="escape"
        aria-label={`Resize ${title}`}
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

/** Every pane, to show any of them when one group fills the dock. */
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
  const shown = useLayout(
    (layout) => groups(layout).find((group) => group.id === layout.focused)?.active ?? "",
  );
  const choose = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => {
      const pane = event.target.value as PaneId;
      change((layout) => activate(layout, pane));
      const found = workbench.getState().layout.panes[pane];
      if (found) {
        kinds.get(found.kind).chosen?.(found);
      }
    },
    [kinds],
  );
  return (
    <select className={css("pane-picker")} aria-label="Pane" value={shown} onChange={choose}>
      {options.split("\u0001").map((option) => {
        const [pane, title] = option.split("\u0000");
        return (
          <option key={pane} value={pane}>
            {title}
          </option>
        );
      })}
    </select>
  );
}
