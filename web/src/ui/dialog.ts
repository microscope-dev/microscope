import { createEffect, onSettled } from "solid-js";

const CONTROLS = "input, select, textarea, button, [href], [tabindex]:not([tabindex='-1'])";

/**
 * Moves focus into a dialog on mount and restores the previously focused element on unmount,
 * unless the user has already moved focus somewhere else outside the dialog.
 */
export function useDialogFocus(
  dialog: () => HTMLElement | null | undefined,
  first?: () => HTMLElement | null | undefined,
): void {
  onSettled(() => {
    const before = document.activeElement;
    const element = dialog();
    if (element && !element.contains(document.activeElement)) {
      (first?.() ?? element.querySelector<HTMLElement>(CONTROLS) ?? element).focus();
    }
    return () => {
      const now = document.activeElement;
      const lost = now === null || now === document.body || !now.isConnected;
      if ((lost || element?.contains(now)) && before instanceof HTMLElement && before.isConnected) {
        before.focus();
      }
    };
  });
}

export function useDismiss(
  anchor: () => HTMLElement | null | undefined,
  active: () => boolean,
  away: () => void,
): void {
  const key = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      away();
    }
  };
  const pointer = (event: PointerEvent) => {
    if (!anchor()?.contains(event.target as Node)) {
      away();
    }
  };
  createEffect(
    () => active(),
    (isActive) => {
      if (!isActive) {
        return;
      }
      window.addEventListener("keydown", key);
      window.addEventListener("pointerdown", pointer);
      return () => {
        window.removeEventListener("keydown", key);
        window.removeEventListener("pointerdown", pointer);
      };
    },
  );
}
