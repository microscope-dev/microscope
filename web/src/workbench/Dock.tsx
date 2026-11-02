// The dock: the layout's tree of splits and groups, filling the space it is
// given, or the one maximized group until it is restored, with floating
// windows above. In a narrow window it shows one group at a time, with a
// list to choose any pane from, and nothing floats.

import {
  memo,
  type PointerEvent,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import { DockContext, type DockSetup, useDock } from "./context";
import { pressWindow, rememberSize } from "./drag";
import { GroupView } from "./Group";
import { activate, dock, type GroupId, groups, type PaneId, type Rect, reframe } from "./layout";
import { NodeView } from "./SplitView";
import { keepOnScreen } from "./snap";
import { change, useLayout, workbench } from "./store";

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
      <div className="dock" ref={element}>
        {narrow ? (
          <div className="dock-tree dock-narrow">
            <PanePicker />
            <GroupView id={focused} />
          </div>
        ) : (
          <>
            <div className="dock-tree">
              {maximized ? <GroupView id={maximized} /> : <NodeView node={root} />}
            </div>
            {size &&
              // In a fixed order, raised by z-index: moving a window's element
              // would drop the pointer it is being dragged by.
              [...windows]
                .sort()
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
function useSize(element: React.RefObject<HTMLDivElement | null>) {
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
  const { kinds } = useDock();
  const found = useLayout((layout) => layout.floating.find((window) => window.group.id === id));
  const title = useLayout((layout) => {
    const group = groups(layout).find((candidate) => candidate.id === id);
    const pane = group?.active ? layout.panes[group.active] : undefined;
    return pane ? kinds.get(pane.kind).title(pane) : "";
  });
  const element = useRef<HTMLDivElement>(null);
  const resizing = useRef<{ x: number; y: number; rect: Rect } | null>(null);
  if (!found) {
    return null;
  }
  const rect = keepOnScreen(found.rect, { x: 0, y: 0, ...bounds });

  const grip = {
    onPointerDown: (event: PointerEvent<HTMLDivElement>) => {
      if (event.button !== 0) {
        return;
      }
      event.preventDefault();
      event.currentTarget.setPointerCapture(event.pointerId);
      resizing.current = { x: event.clientX, y: event.clientY, rect };
    },
    onPointerMove: (event: PointerEvent<HTMLDivElement>) => {
      const from = resizing.current;
      if (from && element.current) {
        const sized = grown(from.rect, event.clientX - from.x, event.clientY - from.y, bounds);
        element.current.style.width = `${sized.width}px`;
        element.current.style.height = `${sized.height}px`;
      }
    },
    onPointerUp: (event: PointerEvent<HTMLDivElement>) => {
      const from = resizing.current;
      resizing.current = null;
      if (from) {
        const sized = grown(from.rect, event.clientX - from.x, event.clientY - from.y, bounds);
        change((layout) => reframe(layout, id, sized));
      }
    },
  };

  return (
    <div
      ref={element}
      className="dock-window"
      role="dialog"
      aria-label={title}
      style={{ left: rect.x, top: rect.y, width: rect.width, height: rect.height, zIndex: layer }}
      onPointerDownCapture={() => {
        // Working in a window raises it.
        const { layout } = workbench.getState();
        if (layout.floating.at(-1)?.group.id !== id) {
          change((now) => reframe(now, id, found.rect));
        }
      }}
    >
      <div
        className="window-bar"
        onPointerDown={(event) => {
          if (element.current && !(event.target as Element).closest("button")) {
            pressWindow(event, id, element.current, rect);
          }
        }}
      >
        <span className="window-title">{title}</span>
        <button
          type="button"
          className="window-dock"
          title="Dock this window on the right"
          aria-label={`Dock ${title}`}
          onClick={() => {
            rememberSize(workbench.getState().layout, id);
            change((layout) => dock(layout, id, { type: "edge", side: "right" }));
          }}
        >
          Dock
        </button>
      </div>
      <GroupView id={id} />
      <div className="window-grip" aria-hidden="true" {...grip} />
    </div>
  );
});

function grown(rect: Rect, dx: number, dy: number, bounds: { width: number; height: number }) {
  return {
    ...rect,
    width: Math.max(LEAST.width, Math.min(bounds.width - rect.x, rect.width + dx)),
    height: Math.max(LEAST.height, Math.min(bounds.height - rect.y, rect.height + dy)),
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
  return (
    <select
      className="pane-picker"
      aria-label="Pane"
      value={shown}
      onChange={(event) => {
        const pane = event.target.value as PaneId;
        change((layout) => activate(layout, pane));
        const found = workbench.getState().layout.panes[pane];
        if (found) {
          kinds.get(found.kind).chosen?.(found);
        }
      }}
    >
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
