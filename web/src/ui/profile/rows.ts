// The keyboard in a tree or list of virtual rows. The container keeps
// focus and names the row it is on with aria-activedescendant, since a
// row scrolled out of view is not rendered and could not hold focus.

import type { Virtualizer } from "@tanstack/react-virtual";
import { useId, useState } from "react";

export function useActiveRow(count: number, virtual: Virtualizer<HTMLDivElement, Element>) {
  const id = useId();
  const [chosen, setChosen] = useState(0);
  const active = Math.max(0, Math.min(chosen, count - 1));
  const clamp = (index: number) => Math.max(0, Math.min(index, count - 1));
  const move = (index: number) => {
    const next = clamp(index);
    setChosen(next);
    virtual.scrollToIndex(next);
  };
  return {
    active,
    move,
    /** Makes a row active without scrolling, as a click does. */
    choose: (index: number) => setChosen(clamp(index)),
    rowId: (index: number) => `${id}-row-${index}`,
    /** Up, Down, Home, and End move; any other key goes to `other`, which
     * says whether it took it. */
    onKeyDown(event: React.KeyboardEvent, other: (key: string) => boolean) {
      if (count === 0) {
        return;
      }
      const to =
        event.key === "ArrowDown"
          ? active + 1
          : event.key === "ArrowUp"
            ? active - 1
            : event.key === "Home"
              ? 0
              : event.key === "End"
                ? count - 1
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
