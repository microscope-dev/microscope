// The layout as a person sees it: each group's tabs, read from the screen's
// tab lists, the shown one marked with a star.

export function readGroups(within: ParentNode = document): string[][] {
  return [...within.querySelectorAll('[role="tablist"][aria-label="Panes"]')].map((list) =>
    [...list.querySelectorAll('[role="tab"]')].map((tab) => {
      const title = tab.querySelector(".tab-title")?.textContent ?? "";
      return tab.getAttribute("aria-selected") === "true" ? `*${title}` : title;
    }),
  );
}
