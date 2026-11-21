import { createEffect, createMemo, createSignal, For, Show } from "solid-js";

import { assert } from "../assert";
import { classOf } from "./classes";
import type { Availability, Menu, Registry } from "./commands";

import styles from "./MenuBar.module.css";

const css = classOf(styles);

export const MENUS: readonly Menu[] = [
  "File",
  "Edit",
  "View",
  "Dashboards",
  "Machines",
  "Go",
  "Run",
  "Debug",
  "Window",
  "Help",
];

interface Props<Id extends string> {
  readonly registry: Registry<Id, never>;
  availability(id: Id): Availability;
  run(id: Id): void;
}

export const MENU_BAR = "menu-bar";

export function MenuBar<Id extends string>(props: Props<Id>) {
  const [open, setOpen] = createSignal<Menu | null>(null);
  let bar: HTMLDivElement | undefined;
  const menus = createMemo(() =>
    MENUS.filter((menu) => props.registry.ids.some((id) => props.registry.get(id).menu === menu)),
  );

  const pointer = (event: PointerEvent) => {
    if (!bar?.contains(event.target as Node)) {
      setOpen(null);
    }
  };

  createEffect(
    () => open(),
    (current) => {
      if (!current) {
        return;
      }
      window.addEventListener("pointerdown", pointer);
      return () => window.removeEventListener("pointerdown", pointer);
    },
  );

  const button = (menu: Menu) => bar?.querySelector<HTMLElement>(`[data-menu="${menu}"]`) ?? null;
  const items = () => [
    ...(bar?.querySelectorAll<HTMLElement>('[role="menu"] [role="menuitem"]') ?? []),
  ];
  const neighbor = (menu: Menu, by: number): Menu => {
    const list = menus();
    const index = list.indexOf(menu);
    assert(index >= 0, `menu ${menu} is not on the bar`);
    return list[(index + by + list.length) % list.length] as Menu;
  };
  const show = (menu: Menu, focusItem: "first" | "last" | null) => {
    setOpen(menu);
    if (focusItem) {
      requestAnimationFrame(() => {
        const listed = items();
        (focusItem === "first" ? listed[0] : listed.at(-1))?.focus();
      });
    } else {
      button(menu)?.focus();
    }
  };
  const shut = (menu: Menu) => {
    setOpen(null);
    button(menu)?.focus();
  };

  const onBar = (menu: Menu) => (event: KeyboardEvent) => {
    const moves: Record<string, () => void> = {
      ArrowRight: () =>
        open() ? show(neighbor(menu, 1), "first") : button(neighbor(menu, 1))?.focus(),
      ArrowLeft: () =>
        open() ? show(neighbor(menu, -1), "first") : button(neighbor(menu, -1))?.focus(),
      ArrowDown: () => show(menu, "first"),
      Enter: () => show(menu, "first"),
      " ": () => show(menu, "first"),
      ArrowUp: () => show(menu, "last"),
      Escape: () => shut(menu),
    };
    const move = moves[event.key];
    if (move) {
      event.preventDefault();
      event.stopPropagation();
      move();
    }
  };
  const onButtonClick = (menu: Menu) => () => (open() === menu ? setOpen(null) : show(menu, null));
  const onButtonEnter = (menu: Menu) => () => open() && open() !== menu && show(menu, null);
  const onMenu = (menu: Menu) => (event: KeyboardEvent) => {
    const listed = items();
    const index = listed.indexOf(document.activeElement as HTMLElement);
    const moves: Record<string, () => void> = {
      ArrowDown: () => listed[(index + 1) % listed.length]?.focus(),
      ArrowUp: () => listed[(index - 1 + listed.length) % listed.length]?.focus(),
      Home: () => listed[0]?.focus(),
      End: () => listed.at(-1)?.focus(),
      ArrowRight: () => show(neighbor(menu, 1), "first"),
      ArrowLeft: () => show(neighbor(menu, -1), "first"),
      Escape: () => shut(menu),
      Tab: () => setOpen(null),
    };
    const move = moves[event.key];
    if (move) {
      if (event.key !== "Tab") {
        event.preventDefault();
      }
      event.stopPropagation();
      move();
    }
  };

  return (
    <div
      class={css("menu-bar")}
      role="menubar"
      aria-label="Menus"
      ref={(el) => {
        bar = el;
      }}
      data-keys="escape"
    >
      <For each={menus()}>
        {(menu, index) => {
          const menuIds = createMemo(() =>
            props.registry.ids.filter((id) => props.registry.get(id).menu === menu),
          );
          return (
            <div class={css("menu-anchor")}>
              <button
                type="button"
                role="menuitem"
                id={index() === 0 ? MENU_BAR : undefined}
                class={[css("menu-button"), { [css("on")]: open() === menu }]}
                aria-haspopup="menu"
                aria-expanded={open() === menu ? "true" : "false"}
                data-menu={menu}
                tabindex={index() === 0 ? 0 : -1}
                onClick={onButtonClick(menu)}
                onPointerEnter={onButtonEnter(menu)}
                onKeyDown={onBar(menu)}
              >
                {menu}
              </button>
              <Show when={open() === menu}>
                {/* oxlint-disable-next-line jsx-a11y/interactive-supports-focus -- its items take the focus, as a menu's do */}
                <div class={css("menu")} role="menu" aria-label={menu} onKeyDown={onMenu(menu)}>
                  <For each={menuIds()}>
                    {(id) => {
                      const command = createMemo(() => props.registry.get(id));
                      const now = createMemo(() => props.availability(id));
                      const key = createMemo(
                        () => props.registry.describe(id) ?? props.registry.describe(id, true),
                      );
                      return (
                        <button
                          type="button"
                          role="menuitem"
                          class={css("menu-item")}
                          tabindex={-1}
                          aria-disabled={now().state === "ready" ? "false" : "true"}
                          aria-label={command().label}
                          aria-keyshortcuts={props.registry.aria(id) ?? undefined}
                          title={
                            now().state === "blocked" ? (now() as { why: string }).why : undefined
                          }
                          onClick={() => {
                            if (props.availability(id).state === "ready") {
                              setOpen(null);
                              props.run(id);
                            }
                          }}
                        >
                          <span>{command().label}</span>
                          <Show when={key()}>{(shortcut) => <kbd>{shortcut()}</kbd>}</Show>
                        </button>
                      );
                    }}
                  </For>
                </div>
              </Show>
            </div>
          );
        }}
      </For>
    </div>
  );
}
