import { type JSX, render as solidRender } from "@solidjs/web";
import { flush } from "solid-js";

interface Mounted {
  dispose: () => void;
  ownedContainer: HTMLElement | null;
}

const mounted = new Set<Mounted>();

export function render(
  ui: () => JSX.Element,
  options?: { container?: HTMLElement },
): { container: HTMLElement; unmount: () => void } {
  const owned = options?.container ? null : document.createElement("div");
  const container = options?.container ?? (owned as HTMLElement);
  if (owned) {
    document.body.append(owned);
  }
  const dispose = solidRender(ui, container);
  flush();
  const entry: Mounted = { dispose, ownedContainer: owned };
  mounted.add(entry);
  return {
    container,
    unmount: () => {
      if (!mounted.delete(entry)) {
        return;
      }
      dispose();
      owned?.remove();
    },
  };
}

export function cleanup(): void {
  for (const entry of mounted) {
    entry.dispose();
    entry.ownedContainer?.remove();
  }
  mounted.clear();
}

export function act(fn: () => void): void {
  fn();
  flush();
}
