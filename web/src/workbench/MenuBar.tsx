// The menu bar: File, Edit, View and the rest, each listing the commands the
// registry puts in it. It has no items of its own. A command that cannot run
// now stays listed, disabled, and says why. It works as an ARIA menubar: the
// arrow keys move along the bar and down a menu, Enter runs, Escape closes.

import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import { assert } from "../assert";
import type { Availability, Menu, Registry } from "./commands";

export const MENUS: readonly Menu[] = [
  "File",
  "Edit",
  "View",
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

/** The id the bar's first menu button goes by, so a command can focus it. */
export const MENU_BAR = "menu-bar";

export function MenuBar<Id extends string>({ registry, availability, run }: Props<Id>) {
  const [open, setOpen] = useState<Menu | null>(null);
  const bar = useRef<HTMLDivElement>(null);
  const menus = MENUS.filter((menu) => registry.ids.some((id) => registry.get(id).menu === menu));

  // A click elsewhere closes the menu.
  useEffect(() => {
    if (!open) {
      return;
    }
    const pointer = (event: PointerEvent) => {
      if (!bar.current?.contains(event.target as Node)) {
        setOpen(null);
      }
    };
    window.addEventListener("pointerdown", pointer);
    return () => window.removeEventListener("pointerdown", pointer);
  }, [open]);

  const button = (menu: Menu) =>
    bar.current?.querySelector<HTMLElement>(`[data-menu="${menu}"]`) ?? null;
  const items = () => [
    ...(bar.current?.querySelectorAll<HTMLElement>('[role="menu"] [role="menuitem"]') ?? []),
  ];
  const neighbor = (menu: Menu, by: number): Menu => {
    const index = menus.indexOf(menu);
    assert(index >= 0, `menu ${menu} is not on the bar`);
    return menus[(index + by + menus.length) % menus.length] as Menu;
  };
  const show = (menu: Menu, focusItem: "first" | "last" | null) => {
    setOpen(menu);
    if (focusItem) {
      // After the menu renders.
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
        open ? show(neighbor(menu, 1), "first") : button(neighbor(menu, 1))?.focus(),
      ArrowLeft: () =>
        open ? show(neighbor(menu, -1), "first") : button(neighbor(menu, -1))?.focus(),
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
    <div className="menu-bar" role="menubar" aria-label="Menus" ref={bar} data-keys="escape">
      {menus.map((menu, index) => (
        <div key={menu} className="menu-anchor">
          <button
            type="button"
            role="menuitem"
            id={index === 0 ? MENU_BAR : undefined}
            className={`menu-button ${open === menu ? "on" : ""}`}
            aria-haspopup="menu"
            aria-expanded={open === menu}
            data-menu={menu}
            tabIndex={index === 0 ? 0 : -1}
            onClick={() => (open === menu ? setOpen(null) : show(menu, null))}
            onPointerEnter={() => open && open !== menu && show(menu, null)}
            onKeyDown={onBar(menu)}
          >
            {menu}
          </button>
          {open === menu && (
            <div className="menu" role="menu" aria-label={menu} onKeyDown={onMenu(menu)}>
              {registry.ids
                .filter((id) => registry.get(id).menu === menu)
                .map((id) => {
                  const command = registry.get(id);
                  const now = availability(id);
                  const key = registry.describe(id) ?? registry.describe(id, true);
                  return (
                    <button
                      key={id}
                      type="button"
                      role="menuitem"
                      className="menu-item"
                      tabIndex={-1}
                      aria-disabled={now.state !== "ready"}
                      aria-label={command.label}
                      aria-keyshortcuts={registry.aria(id) ?? undefined}
                      title={now.state === "blocked" ? now.why : undefined}
                      onClick={() => {
                        if (now.state === "ready") {
                          setOpen(null);
                          run(id);
                        }
                      }}
                    >
                      <span>{command.label}</span>
                      {key && <kbd>{key}</kbd>}
                    </button>
                  );
                })}
            </div>
          )}
        </div>
      ))}
    </div>
  );
}
