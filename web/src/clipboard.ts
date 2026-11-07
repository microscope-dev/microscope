/** Copies `text`, and says whether the browser let it. A page served over
 * plain HTTP from another machine has no clipboard API, as when someone
 * opens `microscope web --listen` on a remote host, so it copies through a
 * selection instead, the one way such a page can. */
export async function copyText(text: string): Promise<boolean> {
  if (navigator.clipboard) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // Refused, as some browsers do without permission: try a selection.
    }
  }

  const focused = document.activeElement;
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  area.style.position = "fixed";
  area.style.opacity = "0";
  document.body.append(area);
  area.select();
  const copied = document.execCommand("copy");
  area.remove();
  if (focused instanceof HTMLElement) {
    focused.focus();
  }
  return copied;
}
