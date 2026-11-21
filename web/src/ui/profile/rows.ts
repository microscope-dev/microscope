import { createMemo, createSignal, createUniqueId, untrack } from "solid-js";

import type { SolidVirtualizer } from "../../virtual";

export type ActiveRow = ReturnType<typeof useActiveRow>;

export function useActiveRow(
  count: () => number,
  virtual: SolidVirtualizer<HTMLDivElement, Element>,
) {
  const id = createUniqueId();
  const [chosen, setChosen] = createSignal(0);
  const clamp = (index: number) => Math.max(0, Math.min(index, count() - 1));
  const active = createMemo(() => clamp(chosen()));
  const move = (index: number) => {
    const next = untrack(() => clamp(index));
    setChosen(next);
    virtual.scrollToIndex(next);
  };
  return {
    active,
    move,

    choose: (index: number) => setChosen(untrack(() => clamp(index))),
    rowId: (index: number) => `${id}-row-${index}`,

    onKeyDown(event: KeyboardEvent, other: (key: string) => boolean) {
      const total = count();
      if (total === 0) {
        return;
      }
      const now = active();
      const to =
        event.key === "ArrowDown"
          ? now + 1
          : event.key === "ArrowUp"
            ? now - 1
            : event.key === "Home"
              ? 0
              : event.key === "End"
                ? total - 1
                : null;
      if (to !== null) {
        move(to);
      } else if (!other(event.key)) {
        return;
      }
      event.preventDefault();
    },
  };
}
