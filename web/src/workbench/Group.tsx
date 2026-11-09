// A group of panes: a strip of tabs and the pane each shows. Panes in tabs
// not shown stay mounted and hidden, so a tab switch is instant and keeps
// their scroll, unless their kind says they are too costly to keep.

import {
  type ComponentType,
  type KeyboardEvent,
  memo,
  type MouseEvent,
  type PointerEvent,
  useCallback,
  useEffect,
  useRef,
} from "react";

import { assert } from "../assert";
import { classOf } from "./classes";
import { panelId, tabId, useDock } from "./context";
import { pressTab } from "./drag";
import type { PaneProps } from "./kinds";
import {
  activate,
  type GroupId,
  groups,
  type Layout,
  linkOf,
  type Pane,
  type PaneId,
} from "./layout";
import { PaneHost } from "./PaneHost";
import { change, useLayout } from "./store";
import { chooseTab, closeTab } from "./tabs";

import styles from "./Group.module.css";

const css = classOf(styles);

function groupIn(layout: Layout, id: GroupId) {
  return groups(layout).find((group) => group.id === id) ?? null;
}

export const GroupView = memo(function GroupView({ id }: { id: GroupId }) {
  const group = useLayout((layout) => groupIn(layout, id));
  const panes = useLayout((layout) => layout.panes);
  const focused = useLayout((layout) => layout.focused === id);
  const { kinds } = useDock();
  const strip = useRef<HTMLDivElement>(null);
  const active = group?.active ?? null;

  // A strip with more tabs than room scrolls, keeping the shown one in view.
  useEffect(() => {
    if (active) {
      const shown = document.querySelector<HTMLElement>(`#${tabId(active)}`);
      if (shown && strip.current?.contains(shown)) {
        const box = strip.current.getBoundingClientRect();
        const at = shown.getBoundingClientRect();
        if (at.left < box.left) {
          strip.current.scrollLeft -= box.left - at.left;
        } else if (at.right > box.right) {
          strip.current.scrollLeft += at.right - box.right;
        }
      }
    }
  }, [active]);

  // Working in a group makes it the one keys and new panes go to.
  const focus = useCallback(() => {
    if (!focused && active) {
      change((layout) => activate(layout, active));
    }
  }, [focused, active]);

  // The arrow keys, Home, and End choose a tab as they go; Delete closes it.
  const walk = useCallback(
    (event: KeyboardEvent) => {
      // Chords with modifiers are commands', such as Alt+→ to the next group.
      if (!group || event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) {
        return;
      }
      const index = group.active === null ? -1 : group.tabs.indexOf(group.active);
      if (event.key === "Delete" && group.active) {
        event.preventDefault();
        closeTab(kinds, group.active);
        return;
      }
      const next = {
        ArrowLeft: index - 1,
        ArrowRight: index + 1,
        Home: 0,
        End: group.tabs.length - 1,
      }[event.key];
      if (next === undefined) {
        return;
      }
      event.preventDefault();
      const pane = group.tabs[(next + group.tabs.length) % group.tabs.length];
      assert(pane !== undefined, `tab ${next} of ${group.tabs.length}`);
      chooseTab(kinds, pane);
      document.querySelector<HTMLElement>(`#${tabId(pane)}`)?.focus();
    },
    [group, kinds],
  );

  if (!group) {
    return null;
  }

  return (
    <section
      className={`${css("group")} ${focused ? css("focused") : ""}`}
      data-group={id}
      // The group keys and new panes go to.
      data-focused={focused || undefined}
    >
      {/* oxlint-disable-next-line jsx-a11y/interactive-supports-focus -- its tabs take the focus, one at a time, as a tab list's do */}
      <div
        className={css("group-tabs")}
        ref={strip}
        role="tablist"
        aria-label="Panes"
        onKeyDown={walk}
        onFocus={focus}
      >
        {group.tabs.map((pane) => {
          const found = panes[pane];
          assert(found, `group ${id} lists ${pane}, which is no pane`);
          return (
            <Tab
              key={pane}
              pane={pane}
              found={found}
              title={kinds.get(found.kind).title(found)}
              shown={pane === group.active}
            />
          );
        })}
      </div>
      {group.tabs.map((pane) => {
        const found = panes[pane];
        assert(found, `group ${id} lists ${pane}, which is no pane`);
        const shown = pane === group.active;
        if (!shown && !kinds.get(found.kind).keepAlive) {
          return null;
        }
        return (
          // oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions -- focus coming from inside marks the group as the one being worked in
          <div
            key={pane}
            className={css("group-panel")}
            role="tabpanel"
            id={panelId(pane)}
            aria-label={kinds.get(found.kind).title(found)}
            tabIndex={-1}
            hidden={!shown}
            onFocus={focus}
          >
            <PaneHost id={pane} pane={found} visible={shown} />
          </div>
        );
      })}
    </section>
  );
});

/** Whether a pane other than the one the keyboard is in reads the link
 * that one reads, so its tab can show they share what is selected. */
function readsFocusedLink(layout: Layout, pane: PaneId, found: Pane): boolean {
  const link = linkOf(found);
  if (link === null) {
    return false;
  }
  const lead = groups(layout).find((group) => group.id === layout.focused)?.active;
  const leading = lead ? layout.panes[lead] : undefined;
  return lead !== pane && leading !== undefined && linkOf(leading) === link;
}

/** One tab in the strip, and its close button if its pane can close. */
function Tab({
  pane,
  found,
  title,
  shown,
}: {
  pane: PaneId;
  found: Pane;
  title: string;
  shown: boolean;
}) {
  const { kinds } = useDock();
  const kind = kinds.get(found.kind);
  const Badge = kind.Badge;
  const linked = useLayout((layout) => readsFocusedLink(layout, pane, found));

  const choose = useCallback(() => chooseTab(kinds, pane), [kinds, pane]);
  const shut = useCallback(() => closeTab(kinds, pane), [kinds, pane]);
  const press = useCallback(
    (event: PointerEvent<HTMLElement>) => pressTab(event, pane, title),
    [pane, title],
  );
  const auxClick = useCallback(
    (event: MouseEvent) => {
      if (event.button === 1) {
        shut();
      }
    },
    [shut],
  );

  // The tab and its close button are pressed as one: either drags the tab, and
  // a middle click on either closes it, without the browser's autoscroll.
  return (
    // oxlint-disable-next-line jsx-a11y/no-static-element-interactions -- pointer handling shared by the two buttons inside, which carry the roles and keys
    <div
      className={`${css("group-tab")} ${shown ? css("on") : ""} ${linked ? css("linked") : ""}`}
      data-part="tab"
      data-linked={linked || undefined}
      onPointerDown={press}
      onMouseDown={noAutoscroll}
      onAuxClick={auxClick}
    >
      <button
        type="button"
        role="tab"
        id={tabId(pane)}
        aria-selected={shown}
        aria-controls={panelId(pane)}
        tabIndex={shown ? 0 : -1}
        title={found.key || title}
        onClick={choose}
      >
        <span className={css("tab-title")} data-part="tab-title">
          {title}
        </span>
        {Badge && <TabBadge Badge={Badge} id={pane} pane={found} needs={kind.needs} />}
      </button>
      {kind.closable(found) && (
        <button
          type="button"
          className={css("group-close")}
          tabIndex={-1}
          aria-label={`Close ${title}`}
          onClick={shut}
        >
          ×
        </button>
      )}
    </div>
  );
}

function noAutoscroll(event: MouseEvent) {
  if (event.button === 1) {
    event.preventDefault();
  }
}

/** A tab's badge, which counts what its pane shows and so needs what the
 * pane needs. */
function TabBadge({
  Badge,
  id,
  pane,
  needs,
}: {
  Badge: ComponentType<PaneProps>;
  id: PaneId;
  pane: Pane;
  needs: string | null;
}) {
  const { useMissing } = useDock();
  // oxlint-disable-next-line react/hooks -- the dock hands down one hook for its whole life
  return useMissing(needs) === null ? <Badge id={id} pane={pane} /> : null;
}
