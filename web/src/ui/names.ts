// How panes name what the program is made of, so each is called the same
// thing everywhere.

/** A thread by its number, and its name when it has one. */
export function threadLabel(id: number, name?: string | null): string {
  return name ? `thread ${id} (${name})` : `thread ${id}`;
}
