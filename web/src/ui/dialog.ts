// Focus for dialogs and popovers: in when one opens, back where it was when
// it closes.

import { type RefObject, useLayoutEffect } from "react";

const CONTROLS = "input, select, textarea, button, [href], [tabindex]:not([tabindex='-1'])";

/**
 * Moves focus into `dialog` while the calling component is mounted, to
 * `first` or else its first control, and back to what had it on unmount.
 * Focus is given back only if nothing else took it meanwhile, so a click
 * that closed the dialog keeps what it clicked.
 */
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
