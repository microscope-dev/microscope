// A group of panes: a strip of tabs and the pane each shows. Panes in tabs
// not shown stay mounted and hidden, so a tab switch is instant and keeps
// their scroll, unless their kind says they are too costly to keep.

import { type KeyboardEvent, memo } from "react";
import { assert } from "../assert";
import { panelId, tabId, useDock } from "./context";
import { activate, type GroupId, groups, type Layout, type PaneId } from "./layout";
import { PaneHost } from "./PaneHost";
import { change, useLayout } from "./store";
import { chooseTab, closeTab } from "./tabs";

function groupIn(layout: Layout, id: GroupId) {
  return groups(layout).find((group) => group.id === id) ?? null;
}

export const GroupView = memo(function GroupView({ id }: { id: GroupId }) {
  const group = useLayout((layout) => groupIn(layout, id));
  const panes = useLayout((layout) => layout.panes);
  const focused = useLayout((layout) => layout.focused === id);
  const { kinds } = useDock();
  if (!group) {
    return null;
  }

  const choose = (pane: PaneId) => chooseTab(kinds, pane);
  const shut = (pane: PaneId) => closeTab(kinds, pane);

  // Working in a group makes it the one keys and new panes go to.
  const focus = () => {
    if (!focused && group.active) {
      const active = group.active;
      change((layout) => activate(layout, active));
    }
  };

  // The arrow keys, Home, and End choose a tab as they go; Delete closes it.
  const walk = (event: KeyboardEvent) => {
    const index = group.active === null ? -1 : group.tabs.indexOf(group.active);
    if (event.key === "Delete" && group.active) {
      event.preventDefault();
      shut(group.active);
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
    choose(pane);
    document.getElementById(tabId(pane))?.focus();
  };

  return (
    <section className={`group ${focused ? "focused" : ""}`} data-group={id}>
      <div
        className="group-tabs"
        role="tablist"
        aria-label="Panes"
        onKeyDown={walk}
        onFocus={focus}
      >
        {group.tabs.map((pane) => {
          const found = panes[pane];
          assert(found, `group ${id} lists ${pane}, which is no pane`);
          const kind = kinds.get(found.kind);
          const title = kind.title(found);
          const shown = pane === group.active;
          const Badge = kind.Badge;
          return (
            <div key={pane} className={`group-tab ${shown ? "on" : ""}`}>
              <button
                type="button"
                role="tab"
                id={tabId(pane)}
                aria-selected={shown}
                aria-controls={panelId(pane)}
                tabIndex={shown ? 0 : -1}
                title={found.key || title}
                onClick={() => choose(pane)}
                // A middle click closes the tab, without the browser's autoscroll.
                onMouseDown={(event) => event.button === 1 && event.preventDefault()}
                onAuxClick={(event) => event.button === 1 && shut(pane)}
              >
                <span className="tab-title">{title}</span>
                {Badge && <Badge id={pane} pane={found} />}
              </button>
              {kind.closable(found) && (
                <button
                  type="button"
                  className="group-close"
                  tabIndex={-1}
                  aria-label={`Close ${title}`}
                  onClick={() => shut(pane)}
                >
                  ×
                </button>
              )}
            </div>
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
          <div
            key={pane}
            className="group-panel"
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
