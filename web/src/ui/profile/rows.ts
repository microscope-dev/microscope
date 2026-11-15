import type { Virtualizer } from "@tanstack/react-virtual";
import { useId, useState } from "react";

export type ActiveRow = ReturnType<typeof useActiveRow>;

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

    choose: (index: number) => setChosen(clamp(index)),
    rowId: (index: number) => `${id}-row-${index}`,

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
