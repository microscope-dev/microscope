import { createEffect } from "solid-js";

const NEAR = 24;

export function useFollowBottom<T extends HTMLElement>(
  content: () => unknown,
  start?: () => unknown,
): { ref: (el: T) => void; onScroll(event: Event & { currentTarget: T }): void } {
  let element: T | undefined;
  let following = true;

  createEffect(
    () => start?.(),
    () => {
      following = true;
    },
  );

  createEffect(
    () => content(),
    () => {
      if (element && following) {
        element.scrollTop = element.scrollHeight;
      }
    },
  );

  return {
    ref: (el: T) => {
      element = el;
    },
    onScroll: (event) => {
      const target = event.currentTarget;
      following = target.scrollHeight - target.scrollTop - target.clientHeight < NEAR;
    },
  };
}
