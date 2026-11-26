import { Dynamic } from "@solidjs/web";
import { type Component, createEffect, createMemo, For, Show } from "solid-js";

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

export function GroupView(props: { id: GroupId }) {
  const group = useLayout((layout) => groupIn(layout, props.id));
  const panes = useLayout((layout) => layout.panes);
  const focused = useLayout((layout) => layout.focused === props.id);
  const { kinds } = useDock();
  let strip: HTMLDivElement | undefined;
  const active = createMemo(() => group()?.active ?? null);

  createEffect(
    () => active(),
    (current) => {
      if (current && strip) {
        const shown = document.querySelector<HTMLElement>(`#${tabId(current)}`);
        if (shown && strip.contains(shown)) {
          const box = strip.getBoundingClientRect();
          const at = shown.getBoundingClientRect();
          if (at.left < box.left) {
            strip.scrollLeft -= box.left - at.left;
          } else if (at.right > box.right) {
            strip.scrollLeft += at.right - box.right;
          }
        }
      }
    },
  );

  const focus = () => {
    const current = active();
    if (!focused() && current) {
      change((layout) => activate(layout, current));
    }
  };

  const walk = (event: KeyboardEvent) => {
    const currentGroup = group();
    if (!currentGroup || event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) {
      return;
    }
    const index =
      currentGroup.active === null ? -1 : currentGroup.tabs.indexOf(currentGroup.active);
    if (event.key === "Delete" && currentGroup.active) {
      event.preventDefault();
      closeTab(kinds, currentGroup.active);
      return;
    }
    const next = {
      ArrowLeft: index - 1,
      ArrowRight: index + 1,
      Home: 0,
      End: currentGroup.tabs.length - 1,
    }[event.key];
    if (next === undefined) {
      return;
    }
    event.preventDefault();
    const pane = currentGroup.tabs[(next + currentGroup.tabs.length) % currentGroup.tabs.length];
    assert(pane !== undefined, `tab ${next} of ${currentGroup.tabs.length}`);
    chooseTab(kinds, pane);
    document.querySelector<HTMLElement>(`#${tabId(pane)}`)?.focus();
  };

  return (
    <Show when={group()}>
      {(currentGroup) => (
        <section
          class={[css("group"), { [css("focused")]: focused() }]}
          data-group={props.id}
          data-focused={focused() || undefined}
        >
          {/* oxlint-disable-next-line jsx-a11y/interactive-supports-focus -- its tabs take the focus, one at a time, as a tab list's do */}
          <div
            class={css("group-tabs")}
            ref={(el) => {
              strip = el;
            }}
            role="tablist"
            aria-label="Panes"
            onKeyDown={walk}
            onFocusIn={focus}
          >
            <For each={currentGroup().tabs}>
              {(pane) => {
                const found = createMemo(() => {
                  const entry = panes()[pane];
                  assert(entry, `group ${props.id} lists ${pane}, which is no pane`);
                  return entry;
                });
                return (
                  <Tab
                    pane={pane}
                    found={found()}
                    title={kinds.get(found().kind).title(found())}
                    shown={pane === currentGroup().active}
                  />
                );
              }}
            </For>
          </div>
          <For each={currentGroup().tabs}>
            {(pane) => {
              const found = createMemo(() => {
                const entry = panes()[pane];
                assert(entry, `group ${props.id} lists ${pane}, which is no pane`);
                return entry;
              });
              const shown = createMemo(() => pane === currentGroup().active);
              const keep = createMemo(() => shown() || kinds.get(found().kind).keepAlive);
              return (
                <Show when={keep()}>
                  <div
                    class={css("group-panel")}
                    role="tabpanel"
                    id={panelId(pane)}
                    aria-label={kinds.get(found().kind).title(found())}
                    tabindex={-1}
                    hidden={!shown()}
                    onFocusIn={focus}
                  >
                    <PaneHost id={pane} pane={found()} visible={shown()} />
                  </div>
                </Show>
              );
            }}
          </For>
        </section>
      )}
    </Show>
  );
}

function readsFocusedLink(layout: Layout, pane: PaneId, found: Pane): boolean {
  const link = linkOf(found);
  if (link === null) {
    return false;
  }
  const lead = groups(layout).find((group) => group.id === layout.focused)?.active;
  const leading = lead ? layout.panes[lead] : undefined;
  return lead !== pane && leading !== undefined && linkOf(leading) === link;
}

function Tab(props: { pane: PaneId; found: Pane; title: string; shown: boolean }) {
  const { kinds } = useDock();
  const kind = createMemo(() => kinds.get(props.found.kind));
  const linked = useLayout((layout) => readsFocusedLink(layout, props.pane, props.found));

  const choose = () => chooseTab(kinds, props.pane);
  const shut = () => closeTab(kinds, props.pane);
  const press = (event: PointerEvent & { currentTarget: HTMLElement }) =>
    pressTab(event, props.pane, props.title);
  const auxClick = (event: MouseEvent) => {
    if (event.button === 1) {
      shut();
    }
  };

  return (
    // oxlint-disable-next-line jsx-a11y/no-static-element-interactions -- pointer handling shared by the two buttons inside, which carry the roles and keys
    <div
      class={[css("group-tab"), { [css("on")]: props.shown, [css("linked")]: linked() }]}
      data-part="tab"
      data-linked={linked() || undefined}
      onPointerDown={press}
      onMouseDown={noAutoscroll}
      onAuxClick={auxClick}
    >
      <button
        type="button"
        role="tab"
        id={tabId(props.pane)}
        aria-selected={props.shown ? "true" : "false"}
        aria-controls={panelId(props.pane)}
        tabindex={props.shown ? 0 : -1}
        title={kind().tooltip?.(props.found) || props.title}
        onClick={choose}
      >
        <span class={css("tab-title")} data-part="tab-title">
          {props.title}
        </span>
        <Show when={kind().Badge}>
          {(Badge) => (
            <TabBadge Badge={Badge()} id={props.pane} pane={props.found} needs={kind().needs} />
          )}
        </Show>
      </button>
      <Show when={kind().closable(props.found)}>
        <button
          type="button"
          class={css("group-close")}
          tabindex={-1}
          aria-label={`Close ${props.title}`}
          onClick={shut}
        >
          ×
        </button>
      </Show>
    </div>
  );
}

function noAutoscroll(event: MouseEvent) {
  if (event.button === 1) {
    event.preventDefault();
  }
}

function TabBadge(props: {
  Badge: Component<PaneProps>;
  id: PaneId;
  pane: Pane;
  needs: string | null;
}) {
  const { useMissing } = useDock();
  const missing = useMissing(
    () => props.needs,
    () => props.pane,
  );
  return (
    <Show when={missing() === null}>
      <Dynamic component={props.Badge} id={props.id} pane={props.pane} />
    </Show>
  );
}
