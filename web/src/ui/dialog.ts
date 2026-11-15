import { type RefObject, useEffect, useLayoutEffect } from "react";

const CONTROLS = "input, select, textarea, button, [href], [tabindex]:not([tabindex='-1'])";

export function useDialogFocus(
  dialog: RefObject<HTMLElement | null>,
  first?: RefObject<HTMLElement | null>,
): void {
  useLayoutEffect(() => {
    const before = document.activeElement;
    const element = dialog.current;
    if (element && !element.contains(document.activeElement)) {
      (first?.current ?? element.querySelector<HTMLElement>(CONTROLS) ?? element).focus();
    }
    return () => {
      const now = document.activeElement;
      const lost = now === null || now === document.body || !now.isConnected;
      if ((lost || element?.contains(now)) && before instanceof HTMLElement && before.isConnected) {
        before.focus();
      }
    };
  }, [dialog, first]);
}

export function useDismiss(
  anchor: RefObject<HTMLElement | null>,
  active: boolean,
  away: () => void,
): void {
  useEffect(() => {
    if (!active) {
      return;
    }
    const key = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        away();
      }
    };
    const pointer = (event: PointerEvent) => {
      if (!anchor.current?.contains(event.target as Node)) {
        away();
      }
    };
    window.addEventListener("keydown", key);
    window.addEventListener("pointerdown", pointer);
    return () => {
      window.removeEventListener("keydown", key);
      window.removeEventListener("pointerdown", pointer);
    };
  }, [anchor, active, away]);
}
