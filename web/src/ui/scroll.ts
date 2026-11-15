import { type RefObject, type UIEvent, useEffect, useRef } from "react";

const NEAR = 24;

export function useFollowBottom<T extends HTMLElement>(
  content: unknown,
  start?: unknown,
): { ref: RefObject<T | null>; onScroll(event: UIEvent<T>): void } {
  const ref = useRef<T>(null);
  const following = useRef(true);

  useEffect(() => {
    following.current = true;
    // oxlint-disable-next-line react/exhaustive-effect-dependencies -- a new start is what follows again
  }, [start]);

  useEffect(() => {
    const element = ref.current;
    if (element && following.current) {
      element.scrollTop = element.scrollHeight;
    }
    // oxlint-disable-next-line react/exhaustive-effect-dependencies -- new content is what moves it
  }, [content]);

  return {
    ref,
    onScroll: (event) => {
      const element = event.currentTarget;
      following.current = element.scrollHeight - element.scrollTop - element.clientHeight < NEAR;
    },
  };
}
