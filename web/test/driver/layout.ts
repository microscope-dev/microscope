export function readGroups(within: ParentNode = document): string[][] {
  return [...within.querySelectorAll('[role="tablist"][aria-label="Panes"]')].map((list) =>
    [...list.querySelectorAll('[role="tab"]')].map((tab) => {
      const title = tab.querySelector("[data-part=tab-title]")?.textContent ?? "";
      return tab.getAttribute("aria-selected") === "true" ? `*${title}` : title;
    }),
  );
}
