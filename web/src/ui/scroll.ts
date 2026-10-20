// Logs that grow at the bottom, such as the console and the program's
// output, follow what is added unless the reader scrolled up to read.

import { type RefObject, type UIEvent, useEffect, useRef } from "react";

/** How near the bottom, in pixels, still counts as at it. */
const NEAR = 24;

/**
 * Scrolls the element given `ref` to its bottom when `content` changes,
 * while the reader is at the bottom. A change of `start`, such as a line
 * the reader just ran, follows even when they had scrolled up.
 */
export function useFollowBottom<T extends HTMLElement>(
  content: unknown,
  start?: unknown,
): { ref: RefObject<T | null>; onScroll(event: UIEvent<T>): void } {
  const ref = useRef<T>(null);
  const following = useRef(true);

  // Declared first, so it runs before the scroll when both change at once.
  // biome-ignore lint/correctness/useExhaustiveDependencies: a new start is what follows again
  useEffect(() => {
    following.current = true;
  }, [start]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: new content is what moves it
  useEffect(() => {
    const element = ref.current;
    if (element && following.current) {
      element.scrollTop = element.scrollHeight;
    }
  }, [content]);

  return {
    ref,
    onScroll: (event) => {
      const element = event.currentTarget;
      following.current = element.scrollHeight - element.scrollTop - element.clientHeight < NEAR;
    },
  };
}
